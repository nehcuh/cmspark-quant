//! Connection pool for managing multiple TDX server connections.
//!
//! Performance optimizations:
//! - Per-server connection stacks (avoid global lock contention)
//! - Bounded total connections via Semaphore
//! - Wait-queue semantics (callers wait when pool is exhausted instead of creating unbounded connections)
//! - Connection pre-warming (min_connections_per_server)
//! - Server failover on connection failure

use crate::client::{TdxClient, TdxConnection};
use crate::commands::setup::setup_connection;
use crate::discovery::{
    detector::{IpDetector, ProbeResult},
    source::{DEFAULT_TDX_SERVERS, ServerInfo, ServerSource, StaticListSource},
    store::{ServerRecord, ServerStore, StoreConfig},
};
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
use tracing::{debug, trace, warn};

/// Connection pool configuration.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Database path for storing server information.
    pub db_path: String,
    /// Maximum number of idle connections per server.
    pub max_connections_per_server: usize,
    /// Minimum number of connections to maintain per server (pre-warming).
    pub min_connections_per_server: usize,
    /// Maximum total active connections across all servers.
    pub max_total_connections: usize,
    /// Connection idle timeout.
    pub idle_timeout: Duration,
    /// Connection maximum lifetime.
    pub max_lifetime: Duration,
    /// Health check interval.
    pub health_check_interval: Duration,
    /// Server refresh interval.
    pub server_refresh_interval: Duration,
    /// Whether to enable background tasks.
    pub enable_background_tasks: bool,
    /// Whether to probe servers concurrently on pool initialization.
    pub auto_probe: bool,
    /// Timeout for waiting when pool is exhausted (0 = no wait, fail immediately).
    pub acquire_timeout: Duration,
    /// Number of ranked servers to try on failover before giving up.
    pub failover_attempts: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            db_path: "rstdx_servers.db".to_string(),
            max_connections_per_server: 5,
            min_connections_per_server: 0,
            max_total_connections: 20,
            idle_timeout: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(300),
            health_check_interval: Duration::from_secs(30),
            server_refresh_interval: Duration::from_secs(600),
            enable_background_tasks: true,
            auto_probe: false,
            acquire_timeout: Duration::from_secs(5),
            failover_attempts: 3,
        }
    }
}

/// Internal pooled connection with metadata.
#[derive(Debug)]
struct PooledConnectionInner {
    conn: TdxConnection,
    created_at: Instant,
    last_used: Instant,
}

impl PooledConnectionInner {
    fn new(conn: TdxConnection) -> Self {
        let now = Instant::now();
        Self {
            conn,
            created_at: now,
            last_used: now,
        }
    }

    fn is_expired(&self, max_lifetime: Duration) -> bool {
        self.created_at.elapsed() > max_lifetime
    }

    fn is_idle(&self, idle_timeout: Duration) -> bool {
        self.last_used.elapsed() > idle_timeout
    }
}

/// Per-server connection stack. Uses a Mutex for fast push/pop.
struct ServerConnStack {
    conns: Vec<PooledConnectionInner>,
}

impl ServerConnStack {
    fn new() -> Self {
        Self { conns: Vec::new() }
    }

    fn pop_valid(&mut self, max_lifetime: Duration, idle_timeout: Duration) -> Option<TdxConnection> {
        while let Some(mut pooled) = self.conns.pop() {
            if pooled.is_expired(max_lifetime) || pooled.is_idle(idle_timeout) {
                trace!("Dropping expired/idle connection from stack");
                continue;
            }
            if !pooled.conn.is_connected() {
                trace!("Dropping disconnected connection from stack");
                continue;
            }
            pooled.last_used = Instant::now();
            return Some(pooled.conn);
        }
        None
    }

    fn push(&mut self, conn: TdxConnection, max_per_server: usize) -> bool {
        if self.conns.len() >= max_per_server {
            return false;
        }
        self.conns.push(PooledConnectionInner::new(conn));
        true
    }

    fn len(&self) -> usize {
        self.conns.len()
    }

    fn cleanup(&mut self, max_lifetime: Duration, idle_timeout: Duration) -> usize {
        let before = self.conns.len();
        self.conns.retain(|c| {
            !c.is_expired(max_lifetime) && !c.is_idle(idle_timeout) && c.conn.is_connected()
        });
        before - self.conns.len()
    }
}

/// Client wrapper that returns the connection to the pool on drop.
#[derive(Debug)]
pub struct PooledClient {
    inner: Option<TdxClient>,
    pool: Arc<PoolInner>,
    key: String,
}

impl Deref for PooledClient {
    type Target = TdxClient;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("PooledClient inner is None")
    }
}

impl DerefMut for PooledClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut().expect("PooledClient inner is None")
    }
}

impl Drop for PooledClient {
    fn drop(&mut self) {
        if let Some(client) = self.inner.take() {
            let conn = client.into_connection();
            let key = self.key.clone();
            let pool = Arc::clone(&self.pool);

            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    pool.return_connection(&key, conn).await;
                });
            } else {
                // No runtime available — release permit to avoid permanent capacity loss
                pool.semaphore.add_permits(1);
            }
        }
    }
}

/// Server ranking for load balancing.
#[derive(Debug, Clone)]
struct ServerRanking {
    host: String,
    port: u16,
}

/// Shared pool state.
struct PoolInner {
    config: PoolConfig,
    store: ServerStore,
    server_rankings: RwLock<Vec<ServerRanking>>,
    /// Per-server connection stacks, keyed by "host:port".
    stacks: RwLock<HashMap<String, Mutex<ServerConnStack>>>,
    /// Bounds total active (checked-out) connections.
    semaphore: Arc<Semaphore>,
    /// Notifies waiters when a connection is returned.
    notify: Arc<Notify>,
}

impl std::fmt::Debug for PoolInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolInner")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PoolInner {
    /// Try to acquire an idle connection for the given server key.
    /// Only locks the specific server's stack (not the global map in write mode).
    async fn acquire_connection(&self, key: &str) -> Option<TdxConnection> {
        let stacks = self.stacks.read().await;
        if let Some(stack_mutex) = stacks.get(key) {
            let mut stack = stack_mutex.lock().await;
            stack.pop_valid(self.config.max_lifetime, self.config.idle_timeout)
        } else {
            None
        }
    }

    /// Return a connection to the pool.
    async fn return_connection(&self, key: &str, conn: TdxConnection) {
        let pushed = {
            let stacks = self.stacks.read().await;
            if let Some(stack_mutex) = stacks.get(key) {
                let mut stack = stack_mutex.lock().await;
                stack.push(conn, self.config.max_connections_per_server)
            } else {
                drop(stacks);
                let mut stacks_write = self.stacks.write().await;
                let stack_mutex = stacks_write
                    .entry(key.to_string())
                    .or_insert_with(|| Mutex::new(ServerConnStack::new()));
                let mut stack = stack_mutex.lock().await;
                stack.push(conn, self.config.max_connections_per_server)
            }
        };

        // Release semaphore permit AFTER pushing so waiters can find the connection
        self.semaphore.add_permits(1);

        if pushed {
            trace!("Returned connection to pool for {}", key);
        } else {
            trace!("Pool full for {}, dropping returned connection", key);
        }

        // Wake a waiter
        self.notify.notify_one();
    }

    /// Remove expired and idle connections across all servers.
    async fn cleanup(&self) {
        let stacks = self.stacks.read().await;
        let mut total_removed = 0;

        for (key, stack_mutex) in stacks.iter() {
            let mut stack = stack_mutex.lock().await;
            let removed = stack.cleanup(self.config.max_lifetime, self.config.idle_timeout);
            if removed > 0 {
                trace!("Cleaned up {} connections for {}", removed, key);
                total_removed += removed;
            }
        }

        if total_removed > 0 {
            debug!("Pool cleanup removed {} expired/idle connections", total_removed);
        }
    }
}

/// Connection pool for TDX servers.
#[derive(Debug)]
pub struct ConnectionPool {
    inner: Arc<PoolInner>,
    #[allow(dead_code)]
    cleanup_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ConnectionPool {
    /// Create a new connection pool with default config.
    ///
    /// # Example
    /// ```no_run
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let pool = rstdx::client::ConnectionPool::new().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new() -> Result<Self> {
        Self::with_config(PoolConfig::default()).await
    }

    /// Create a new connection pool with custom config.
    pub async fn with_config(config: PoolConfig) -> Result<Self> {
        let store = ServerStore::with_config(StoreConfig {
            path: config.db_path.clone(),
            ..Default::default()
        })
        .await?;

        let semaphore = Arc::new(Semaphore::new(config.max_total_connections));
        let notify = Arc::new(Notify::new());

        let inner = Arc::new(PoolInner {
            store,
            config: config.clone(),
            server_rankings: RwLock::new(Vec::new()),
            stacks: RwLock::new(HashMap::new()),
            semaphore,
            notify,
        });

        let pool = Self {
            inner: Arc::clone(&inner),
            cleanup_handle: None,
        };

        // Initialize with default servers if none exist
        let existing_servers: Vec<ServerRecord> = pool.inner.store.get_all_servers().await?;
        if existing_servers.is_empty() {
            pool.initialize_default_servers().await?;
        }

        // Update server rankings
        let _ = pool.update_rankings().await;

        // Auto-probe servers if configured
        if config.auto_probe {
            let _ = pool.refresh_servers().await;
        }

        // Pre-warm connections if configured
        if config.min_connections_per_server > 0 {
            pool.prewarm().await;
        }

        // Start background cleanup task if enabled
        let cleanup_handle = if config.enable_background_tasks {
            let interval = config.health_check_interval;
            let inner_clone = Arc::clone(&inner);
            Some(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // Skip first immediate tick
                loop {
                    ticker.tick().await;
                    inner_clone.cleanup().await;
                }
            }))
        } else {
            None
        };

        Ok(Self {
            inner: Arc::clone(&inner),
            cleanup_handle,
        })
    }

    /// Pre-warm connections to the top-ranked servers.
    /// Connections are placed directly into stacks without touching the semaphore,
    /// since pre-warmed connections are idle (not checked out).
    async fn prewarm(&self) {
        let rankings = self.inner.server_rankings.read().await;
        let servers_to_warm: Vec<_> = rankings
            .iter()
            .take(3)
            .cloned()
            .collect();
        drop(rankings);

        for server in servers_to_warm {
            let min = self.inner.config.min_connections_per_server;
            let key = format!("{}:{}", server.host, server.port);
            let addr = key.clone();

            for _ in 0..min {
                match TdxConnection::connect(&addr).await {
                    Ok(mut conn) => {
                        if setup_connection(&mut conn).await.is_ok() {
                            // Place directly into the stack (no semaphore manipulation)
                            let stacks = self.inner.stacks.read().await;
                            if let Some(stack_mutex) = stacks.get(&key) {
                                let mut stack = stack_mutex.lock().await;
                                stack.push(conn, self.inner.config.max_connections_per_server);
                            } else {
                                drop(stacks);
                                let mut stacks_write = self.inner.stacks.write().await;
                                let stack_mutex = stacks_write
                                    .entry(key.clone())
                                    .or_insert_with(|| Mutex::new(ServerConnStack::new()));
                                let mut stack = stack_mutex.lock().await;
                                stack.push(conn, self.inner.config.max_connections_per_server);
                            }
                            debug!("Pre-warmed connection to {}", key);
                        }
                    }
                    Err(e) => {
                        trace!("Pre-warm failed for {}: {}", key, e);
                        break;
                    }
                }
            }
        }
    }

    /// Initialize default TDX servers.
    async fn initialize_default_servers(&self) -> Result<()> {
        let static_source = StaticListSource::from_list(DEFAULT_TDX_SERVERS);
        let servers = static_source.fetch_servers().await?;

        for server in servers {
            self.inner
                .store
                .upsert_server(
                    &server.host,
                    server.port,
                    server.region.as_deref(),
                    &server.source,
                )
                .await
                .ok();
        }

        Ok(())
    }

    /// Update server rankings based on latency data.
    async fn update_rankings(&self) -> Result<()> {
        let servers: Vec<ServerRecord> = self.inner.store.get_available_servers(50).await?;

        let rankings = servers
            .into_iter()
            .map(|s| ServerRanking {
                host: s.host.clone(),
                port: s.port(),
            })
            .collect();

        let mut rankings_lock = self.inner.server_rankings.write().await;
        *rankings_lock = rankings;

        Ok(())
    }

    /// Get the best server from rankings.
    pub async fn best_server(&self) -> Option<(String, u16)> {
        let rankings = self.inner.server_rankings.read().await;

        if let Some(first) = rankings.first() {
            return Some((first.host.clone(), first.port));
        }

        // Fallback: get any server from database
        if let Ok(servers) = self.inner.store.get_all_servers().await
            && let Some(first) = servers.first()
        {
            return Some((first.host.clone(), first.port()));
        }

        None
    }

    /// Get a pooled connection to the best available server, with failover.
    ///
    /// Tries ranked servers in order. If all fail, returns the last error.
    /// Respects `max_total_connections` — blocks until a permit is available
    /// or `acquire_timeout` expires.
    pub async fn get_connection(&self) -> Result<PooledClient> {
        // Acquire a global concurrency permit
        let permit_result = if self.inner.config.acquire_timeout.is_zero() {
            match self.inner.semaphore.try_acquire() {
                Ok(permit) => {
                    permit.forget();
                    Ok(())
                }
                Err(_) => Err(Error::Connection("pool exhausted, no connections available".into())),
            }
        } else {
            match tokio::time::timeout(
                self.inner.config.acquire_timeout,
                self.inner.semaphore.acquire(),
            )
            .await
            {
                Ok(Ok(permit)) => {
                    permit.forget(); // We manage permits manually via return_connection
                    Ok(())
                }
                Ok(Err(_)) => Err(Error::PoolClosed),
                Err(_) => Err(Error::Connection("acquire timeout: pool exhausted".into())),
            }
        };

        if let Err(e) = permit_result {
            return Err(e);
        }

        // Try servers in ranked order with failover
        let rankings = self.inner.server_rankings.read().await;
        let servers: Vec<_> = rankings
            .iter()
            .take(self.inner.config.failover_attempts)
            .cloned()
            .collect();
        drop(rankings);

        let mut last_err = None;
        for server in &servers {
            match self.try_get_connection_inner(&server.host, server.port).await {
                Ok(client) => return Ok(client),
                Err(e) => {
                    warn!("Failover: {} failed: {}", format!("{}:{}", server.host, server.port), e);
                    last_err = Some(e);
                }
            }
        }

        // All ranked servers failed — release the permit
        self.inner.semaphore.add_permits(1);
        Err(last_err.unwrap_or(Error::NoAvailableServer))
    }

    /// Get a pooled connection to a specific server.
    pub async fn get_connection_to(&self, host: &str, port: u16) -> Result<PooledClient> {
        // Acquire permit
        match tokio::time::timeout(
            self.inner.config.acquire_timeout,
            self.inner.semaphore.acquire(),
        )
        .await
        {
            Ok(Ok(permit)) => permit.forget(),
            Ok(Err(_)) => return Err(Error::PoolClosed),
            Err(_) => return Err(Error::Connection("acquire timeout: pool exhausted".into())),
        }

        match self.try_get_connection_inner(host, port).await {
            Ok(client) => Ok(client),
            Err(e) => {
                self.inner.semaphore.add_permits(1);
                Err(e)
            }
        }
    }

    /// Inner helper: try to get a connection (from pool or fresh) without managing the semaphore.
    async fn try_get_connection_inner(&self, host: &str, port: u16) -> Result<PooledClient> {
        let key = format!("{}:{}", host, port);

        // Try to reuse an existing idle connection
        if let Some(conn) = self.inner.acquire_connection(&key).await {
            let client = TdxClient::from_connection(conn);
            return Ok(PooledClient {
                inner: Some(client),
                pool: Arc::clone(&self.inner),
                key,
            });
        }

        // Create a new connection
        let client = TdxClient::connect(&format!("{}:{}", host, port)).await?;

        Ok(PooledClient {
            inner: Some(client),
            pool: Arc::clone(&self.inner),
            key,
        })
    }

    /// Create a new connection to the best available server (not pooled).
    pub async fn connect(&self) -> Result<TdxClient> {
        let (host, port) = self.best_server().await.ok_or(Error::NoAvailableServer)?;
        self.connect_to(&host, port).await
    }

    /// Create a new connection to a specific server (not pooled).
    pub async fn connect_to(&self, host: &str, port: u16) -> Result<TdxClient> {
        let addr = format!("{}:{}", host, port);
        TdxClient::connect(&addr).await
    }

    /// Refresh server list and probe all servers concurrently.
    pub async fn refresh_servers(&self) -> Result<usize> {
        let servers: Vec<ServerRecord> = self.inner.store.get_all_servers().await?;

        let server_infos: Vec<ServerInfo> = servers
            .iter()
            .map(|s| ServerInfo {
                host: s.host.clone(),
                port: s.port(),
                region: s.region.clone(),
                source: s.source.clone(),
            })
            .collect();

        let detector = IpDetector::new(self.inner.store.clone_for_pool()).await?;
        let results: Vec<ProbeResult> = detector.probe_and_store(server_infos).await?;

        let _ = self.update_rankings().await;

        Ok(results.len())
    }

    /// Get available servers from the store.
    pub async fn available_servers(&self) -> Result<Vec<ServerRecord>> {
        self.inner.store.get_available_servers(50).await
    }

    /// Get all servers (including those without recent stats).
    pub async fn all_servers(&self) -> Result<Vec<ServerRecord>> {
        self.inner.store.get_all_servers().await
    }

    /// Get current pool statistics.
    pub async fn stats(&self) -> PoolStats {
        let stacks = self.inner.stacks.read().await;
        let mut total_idle = 0;
        let mut servers = 0;

        for (_key, stack_mutex) in stacks.iter() {
            let stack = stack_mutex.lock().await;
            let n = stack.len();
            if n > 0 {
                servers += 1;
                total_idle += n;
            }
        }

        let available_permits = self.inner.semaphore.available_permits();
        let active = self.inner.config.max_total_connections.saturating_sub(available_permits);

        PoolStats {
            total_idle_connections: total_idle,
            active_connections: active,
            servers_with_connections: servers,
            max_total_connections: self.inner.config.max_total_connections,
        }
    }

    /// Run manual cleanup of expired/idle connections.
    pub async fn cleanup(&self) {
        self.inner.cleanup().await;
    }

    /// Wait for a connection to become available (for callers that want
    /// to poll rather than block on `get_connection`).
    pub async fn wait_for_available(&self, timeout: Duration) -> bool {
        if self.inner.semaphore.available_permits() > 0 {
            return true;
        }
        tokio::time::timeout(timeout, self.inner.notify.notified())
            .await
            .is_ok()
    }
}

/// Pool statistics snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStats {
    /// Total number of idle connections in the pool.
    pub total_idle_connections: usize,
    /// Number of currently checked-out connections.
    pub active_connections: usize,
    /// Number of servers that have idle connections.
    pub servers_with_connections: usize,
    /// Maximum total connections allowed.
    pub max_total_connections: usize,
}

/// Clone helper for ServerStore.
impl ServerStore {
    pub(crate) fn clone_for_pool(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            config: self.config.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_create() {
        let pool = ConnectionPool::new().await.unwrap();
        drop(pool);
    }

    #[test]
    fn test_pool_config_default() {
        let config = PoolConfig::default();
        assert_eq!(config.max_connections_per_server, 5);
        assert_eq!(config.min_connections_per_server, 0);
        assert_eq!(config.max_total_connections, 20);
        assert_eq!(config.db_path, "rstdx_servers.db");
        assert!(config.enable_background_tasks);
        assert!(!config.auto_probe);
        assert_eq!(config.failover_attempts, 3);
    }

    #[tokio::test]
    async fn test_pool_best_server_empty() {
        let pool = ConnectionPool::new().await.unwrap();
        let best = pool.best_server().await;
        assert!(best.is_some());
    }

    #[tokio::test]
    async fn test_pool_available_servers_empty_without_probe() {
        let pool = ConnectionPool::new().await.unwrap();
        let servers = pool.available_servers().await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn test_pool_all_servers() {
        let pool = ConnectionPool::new().await.unwrap();
        let servers = pool.all_servers().await.unwrap();
        assert!(!servers.is_empty());
    }

    #[tokio::test]
    async fn test_pool_connect_to_specific() {
        let pool = ConnectionPool::new().await.unwrap();
        let result = pool.connect_to("127.0.0.1", 7709).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_pool_stats_empty() {
        let pool = ConnectionPool::new().await.unwrap();
        let stats = pool.stats().await;
        assert_eq!(stats.total_idle_connections, 0);
        assert_eq!(stats.active_connections, 0);
        assert_eq!(stats.servers_with_connections, 0);
        assert_eq!(stats.max_total_connections, 20);
    }

    #[tokio::test]
    async fn test_pool_cleanup_empty() {
        let pool = ConnectionPool::new().await.unwrap();
        pool.cleanup().await;
        let stats = pool.stats().await;
        assert_eq!(stats.total_idle_connections, 0);
    }

    #[tokio::test]
    async fn test_pool_wait_for_available_immediately() {
        let pool = ConnectionPool::new().await.unwrap();
        // Should return true immediately since no connections are checked out
        let available = pool.wait_for_available(Duration::from_millis(10)).await;
        assert!(available);
    }

    fn create_test_connection() -> TdxConnection {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let _ = listener.accept().await.unwrap();
            TdxConnection::from_stream(stream)
        })
    }

    #[test]
    fn test_pooled_connection_inner_expired() {
        let conn = create_test_connection();
        let pooled = PooledConnectionInner::new(conn);
        std::thread::sleep(Duration::from_millis(10));
        assert!(pooled.is_expired(Duration::from_secs(0)));
        assert!(!pooled.is_expired(Duration::from_secs(3600)));
    }

    #[test]
    fn test_pooled_connection_inner_idle() {
        let conn = create_test_connection();
        let pooled = PooledConnectionInner::new(conn);
        std::thread::sleep(Duration::from_millis(10));
        assert!(pooled.is_idle(Duration::from_secs(0)));
        assert!(!pooled.is_idle(Duration::from_secs(3600)));
    }

    #[test]
    fn test_server_conn_stack_push_pop() {
        let conn = create_test_connection();
        let mut stack = ServerConnStack::new();
        assert!(stack.push(conn, 5));
        assert_eq!(stack.len(), 1);
        let popped = stack.pop_valid(Duration::from_secs(3600), Duration::from_secs(3600));
        assert!(popped.is_some());
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn test_server_conn_stack_max_limit() {
        let mut stack = ServerConnStack::new();
        for _ in 0..3 {
            let conn = create_test_connection();
            stack.push(conn, 3);
        }
        assert_eq!(stack.len(), 3);
        let extra = create_test_connection();
        assert!(!stack.push(extra, 3)); // Should reject (at max)
    }

    #[test]
    fn test_pool_stats_display() {
        let stats = PoolStats {
            total_idle_connections: 3,
            active_connections: 2,
            servers_with_connections: 2,
            max_total_connections: 20,
        };
        assert_eq!(
            format!("{:?}", stats),
            "PoolStats { total_idle_connections: 3, active_connections: 2, servers_with_connections: 2, max_total_connections: 20 }"
        );
    }

    #[tokio::test]
    async fn test_pool_inner_return_and_acquire() {
        let config = PoolConfig::default();
        let store = ServerStore::with_config(StoreConfig {
            path: ":memory:".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

        let inner = Arc::new(PoolInner {
            config: config.clone(),
            store,
            server_rankings: RwLock::new(Vec::new()),
            stacks: RwLock::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(config.max_total_connections)),
            notify: Arc::new(Notify::new()),
        });

        let conn = create_test_connection();
        inner.return_connection("test:1234", conn).await;

        let acquired = inner.acquire_connection("test:1234").await;
        assert!(acquired.is_some());

        // After acquiring, stack should be empty
        let acquired2 = inner.acquire_connection("test:1234").await;
        assert!(acquired2.is_none());
    }
}
