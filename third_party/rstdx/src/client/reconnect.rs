//! Auto-reconnect mechanism for TDX connections.

use crate::client::TdxConnection;
use crate::error::{Error, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Reconnect configuration.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// Initial backoff duration.
    pub initial_backoff: Duration,
    /// Maximum backoff duration.
    pub max_backoff: Duration,
    /// Backoff multiplier (e.g., 2.0 for exponential backoff).
    pub backoff_multiplier: f64,
    /// Maximum number of retry attempts (None = infinite).
    pub max_retries: Option<usize>,
    /// Whether to enable auto-reconnect.
    pub enabled: bool,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            max_retries: Some(10),
            enabled: true,
        }
    }
}

/// Reconnect policy result.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconnectAction {
    /// Retry the connection.
    Retry { delay: Duration, attempt: usize },
    /// Give up on reconnection.
    GiveUp,
    /// Reconnection successful.
    Success,
}

/// Reconnect manager for handling connection failures.
#[derive(Debug)]
pub struct ReconnectManager {
    config: ReconnectConfig,
    retry_count: Arc<AtomicU64>,
    is_reconnecting: Arc<AtomicBool>,
}

impl Default for ReconnectManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ReconnectManager {
    /// Create a new reconnect manager with default config.
    pub fn new() -> Self {
        Self::with_config(ReconnectConfig::default())
    }

    /// Create a new reconnect manager with custom config.
    pub fn with_config(config: ReconnectConfig) -> Self {
        Self {
            config,
            retry_count: Arc::new(AtomicU64::new(0)),
            is_reconnecting: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Calculate the next backoff delay based on retry attempt.
    fn calculate_backoff(&self, attempt: usize) -> Duration {
        let base_ms = self.config.initial_backoff.as_millis() as f64;
        let multiplier = self.config.backoff_multiplier;
        let exponential_ms = base_ms * multiplier.powi(attempt as i32);
        let backoff_ms = exponential_ms.min(self.config.max_backoff.as_millis() as f64);
        Duration::from_millis(backoff_ms as u64)
    }

    /// Determine the reconnect action based on the error and retry history.
    pub fn on_connection_error(&self, error: &Error) -> ReconnectAction {
        if !self.config.enabled {
            return ReconnectAction::GiveUp;
        }

        // Check if we should give up based on error type
        match error {
            // These errors are not recoverable
            Error::NoAvailableServer | Error::PoolClosed => {
                debug!("Non-recoverable error: {}", error);
                return ReconnectAction::GiveUp;
            }
            // Network errors are recoverable
            Error::Io(_) | Error::Connection(_) | Error::Protocol(_) => {
                // Continue to retry logic
            }
            // Other errors might be recoverable
            _ => {}
        }

        let attempt = self.retry_count.load(Ordering::Relaxed) as usize;

        // Check max retry limit
        if let Some(max) = self.config.max_retries
            && attempt >= max
        {
            warn!("Max retry attempts ({}) reached, giving up", max);
            return ReconnectAction::GiveUp;
        }

        let delay = self.calculate_backoff(attempt);
        info!(
            "Connection failed, retrying in {:?} (attempt {})",
            delay,
            attempt + 1
        );

        ReconnectAction::Retry { delay, attempt }
    }

    /// Record a successful connection (resets retry counter).
    pub fn on_connection_success(&self) {
        self.retry_count.store(0, Ordering::Relaxed);
        self.is_reconnecting.store(false, Ordering::Relaxed);
    }

    /// Record a reconnection attempt.
    fn record_attempt(&self) {
        self.retry_count.fetch_add(1, Ordering::Relaxed);
        self.is_reconnecting.store(true, Ordering::Relaxed);
    }

    /// Get the current retry count.
    pub fn retry_count(&self) -> u64 {
        self.retry_count.load(Ordering::Relaxed)
    }

    /// Check if currently reconnecting.
    pub fn is_reconnecting(&self) -> bool {
        self.is_reconnecting.load(Ordering::Relaxed)
    }

    /// Reset the retry counter.
    pub fn reset(&self) {
        self.retry_count.store(0, Ordering::Relaxed);
        self.is_reconnecting.store(false, Ordering::Relaxed);
    }

    /// Attempt to reconnect with backoff.
    ///
    /// This function will:
    /// 1. Check if reconnection should be attempted
    /// 2. Wait for the calculated backoff duration
    /// 3. Attempt to establish a new connection
    /// 4. Return the new connection or error
    pub async fn reconnect_with_backoff<F, Fut>(&self, mut connect_fn: F) -> Result<TdxConnection>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<TdxConnection>>,
    {
        loop {
            // Try to connect
            let result = connect_fn().await;

            match result {
                Ok(conn) => {
                    let attempts = self.retry_count();
                    self.on_connection_success();
                    info!(
                        "Reconnection successful after {} attempts",
                        attempts
                    );
                    return Ok(conn);
                }
                Err(e) => {
                    let action = self.on_connection_error(&e);

                    match action {
                        ReconnectAction::Retry { delay, .. } => {
                            self.record_attempt();
                            warn!(
                                "Reconnection failed, waiting {:?} before retry: {}",
                                delay, e
                            );
                            sleep(delay).await;
                            // Continue loop to retry
                        }
                        ReconnectAction::GiveUp => {
                            self.is_reconnecting.store(false, Ordering::Relaxed);
                            return Err(e);
                        }
                        ReconnectAction::Success => {
                            unreachable!("Success should not be returned from on_connection_error")
                        }
                    }
                }
            }
        }
    }

    /// Attempt to reconnect to a list of servers.
    ///
    /// This will try each server in order, with backoff between attempts.
    pub async fn reconnect_to_servers<F, Fut>(
        &self,
        servers: Vec<String>,
        mut connect_fn: F,
    ) -> Result<TdxConnection>
    where
        F: FnMut(&str) -> Fut,
        Fut: std::future::Future<Output = Result<TdxConnection>>,
    {
        if servers.is_empty() {
            return Err(Error::NoAvailableServer);
        }

        let mut last_error: Option<Error> = None;

        for server in servers.iter().cycle() {
            // Check if we should give up (skip on first iteration)
            if let Some(ref err) = last_error {
                let action = self.on_connection_error(err);
                if matches!(action, ReconnectAction::GiveUp) {
                    return Err(last_error.unwrap());
                }
            }

            // Try to connect to this server
            match connect_fn(server).await {
                Ok(conn) => {
                    self.on_connection_success();
                    info!("Reconnected to {}", server);
                    return Ok(conn);
                }
                Err(e) => {
                    let action = self.on_connection_error(&e);
                    last_error = Some(e);

                    if let ReconnectAction::Retry { delay, .. } = action {
                        self.record_attempt();
                        warn!(
                            "Failed to connect to {}, waiting {:?} before retry",
                            server, delay
                        );
                        sleep(delay).await;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
            }
        }

        Err(last_error.unwrap_or(Error::NoAvailableServer))
    }
}

/// Wrapper for connections with auto-reconnect capability.
pub struct ReconnectingClient {
    conn: Option<TdxConnection>,
    reconnect: ReconnectManager,
    server_addr: String,
}

impl ReconnectingClient {
    /// Create a new reconnecting client.
    pub async fn connect(addr: &str) -> Result<Self> {
        Self::connect_with_config(addr, ReconnectConfig::default()).await
    }

    /// Create a new reconnecting client with custom config.
    pub async fn connect_with_config(addr: &str, config: ReconnectConfig) -> Result<Self> {
        let conn = Self::connect_to_server(addr).await?;
        let reconnect = ReconnectManager::with_config(config);

        Ok(Self {
            conn: Some(conn),
            reconnect,
            server_addr: addr.to_string(),
        })
    }

    /// Helper to connect to a server.
    async fn connect_to_server(addr: &str) -> Result<TdxConnection> {
        let mut conn = TdxConnection::connect(addr).await?;
        crate::commands::setup_connection(&mut conn).await?;
        Ok(conn)
    }

    /// Get the current connection, reconnecting if necessary.
    pub async fn connection(&mut self) -> Result<&mut TdxConnection> {
        // Check if connection is alive by attempting a small operation
        // For now, we assume the connection is good if we have one
        if self.conn.is_none() {
            self.reconnect().await?;
        }

        self.conn
            .as_mut()
            .ok_or(Error::Connection("No connection available".to_string()))
    }

    /// Force a reconnection.
    pub async fn reconnect(&mut self) -> Result<()> {
        let addr = self.server_addr.clone();
        let reconnect = &self.reconnect;

        let new_conn = reconnect
            .reconnect_with_backoff(|| Self::connect_to_server(&addr))
            .await?;

        self.conn = Some(new_conn);
        Ok(())
    }

    /// Execute an operation with automatic reconnect on failure.
    ///
    /// Note: The operation function should be idempotent or handle retries gracefully,
    /// as it may be called multiple times if reconnection occurs.
    pub async fn execute_with_reconnect<F, Fut, R>(&mut self, mut f: F) -> Result<R>
    where
        F: FnMut(&mut TdxConnection) -> Fut,
        Fut: std::future::Future<Output = Result<R>>,
    {
        loop {
            // Get current connection
            let conn = self.connection().await?;

            // Try to execute the operation
            match f(conn).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    // Check if this is a recoverable error
                    if matches!(e, Error::Io(_) | Error::Connection(_) | Error::Protocol(_)) {
                        warn!("Operation failed, attempting reconnect: {}", e);
                        self.conn = None; // Force reconnect on next attempt

                        // Try to reconnect
                        let addr = self.server_addr.clone();
                        let reconnect = &self.reconnect;

                        match reconnect
                            .reconnect_with_backoff(|| Self::connect_to_server(&addr))
                            .await
                        {
                            Ok(new_conn) => {
                                self.conn = Some(new_conn);
                                // Continue loop to retry the operation
                            }
                            Err(reconnect_err) => {
                                return Err(reconnect_err);
                            }
                        }
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Get the reconnect manager.
    pub fn reconnect_manager(&self) -> &ReconnectManager {
        &self.reconnect
    }

    /// Reset the reconnect state.
    pub fn reset_reconnect(&self) {
        self.reconnect.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconnect_config_default() {
        let config = ReconnectConfig::default();
        assert_eq!(config.initial_backoff, Duration::from_millis(500));
        assert_eq!(config.max_backoff, Duration::from_secs(30));
        assert_eq!(config.backoff_multiplier, 2.0);
        assert_eq!(config.max_retries, Some(10));
        assert!(config.enabled);
    }

    #[test]
    fn test_calculate_backoff() {
        let manager = ReconnectManager::new();

        // First retry: 500ms
        let backoff0 = manager.calculate_backoff(0);
        assert_eq!(backoff0, Duration::from_millis(500));

        // Second retry: 1000ms
        let backoff1 = manager.calculate_backoff(1);
        assert_eq!(backoff1, Duration::from_millis(1000));

        // Third retry: 2000ms
        let backoff2 = manager.calculate_backoff(2);
        assert_eq!(backoff2, Duration::from_millis(2000));

        // Should not exceed max_backoff (30s)
        let backoff_large = manager.calculate_backoff(100);
        assert!(backoff_large <= Duration::from_secs(30));
    }

    #[test]
    fn test_on_connection_error_give_up() {
        let config = ReconnectConfig {
            max_retries: Some(0),
            ..Default::default()
        };
        let manager = ReconnectManager::with_config(config);

        let action = manager.on_connection_error(&Error::Connection("test".to_string()));
        assert_eq!(action, ReconnectAction::GiveUp);
    }

    #[test]
    fn test_on_connection_error_retry() {
        let manager = ReconnectManager::new();

        let action = manager.on_connection_error(&Error::Connection("test".to_string()));
        assert!(matches!(action, ReconnectAction::Retry { .. }));
    }

    #[test]
    fn test_on_connection_error_non_recoverable() {
        let manager = ReconnectManager::new();

        let action = manager.on_connection_error(&Error::NoAvailableServer);
        assert_eq!(action, ReconnectAction::GiveUp);
    }

    #[test]
    fn test_reset() {
        let manager = ReconnectManager::new();
        manager.retry_count.fetch_add(5, Ordering::Relaxed);

        assert_eq!(manager.retry_count(), 5);
        manager.reset();
        assert_eq!(manager.retry_count(), 0);
    }

    #[test]
    fn test_reconnect_config_custom() {
        let config = ReconnectConfig {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            backoff_multiplier: 1.5,
            max_retries: Some(5),
            enabled: false,
        };
        assert_eq!(config.initial_backoff, Duration::from_secs(1));
        assert_eq!(config.max_retries, Some(5));
        assert!(!config.enabled);
    }

    #[test]
    fn test_calculate_backoff_max_limit() {
        let config = ReconnectConfig {
            max_backoff: Duration::from_secs(10),
            ..Default::default()
        };
        let manager = ReconnectManager::with_config(config);
        let backoff = manager.calculate_backoff(100);
        assert!(backoff <= Duration::from_secs(10));
    }

    #[test]
    fn test_reconnect_manager_disabled() {
        let config = ReconnectConfig {
            enabled: false,
            ..Default::default()
        };
        let manager = ReconnectManager::with_config(config);
        let action = manager.on_connection_error(&Error::Connection("test".to_string()));
        assert_eq!(action, ReconnectAction::GiveUp);
    }

    #[test]
    fn test_is_reconnecting() {
        let manager = ReconnectManager::new();
        assert!(!manager.is_reconnecting());
    }
}
