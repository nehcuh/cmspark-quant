//! SQLite storage for server information and statistics.

use crate::error::Result;
use chrono::{DateTime, Utc};
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::str::FromStr;

/// Store configuration.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Database file path.
    pub path: String,
    /// Whether to enable WAL mode for better concurrency.
    pub enable_wal: bool,
    /// Statistics data retention duration.
    pub stats_retention: chrono::Duration,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            path: "rstdx_servers.db".to_string(),
            enable_wal: true,
            stats_retention: chrono::Duration::days(7),
        }
    }
}

/// Server record from database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ServerRecord {
    pub id: i64,
    pub host: String,
    pub port: i64,
    pub region: Option<String>,
    pub source: String,
    pub created_at: i64,
    pub last_checked_at: Option<i64>,
    pub avg_latency: Option<f64>,
    pub min_latency: Option<f64>,
    pub check_count: i64,
}

impl ServerRecord {
    pub fn port(&self) -> u16 {
        self.port as u16
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.created_at, 0).unwrap_or_default()
    }

    pub fn last_checked_at(&self) -> Option<DateTime<Utc>> {
        self.last_checked_at
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
    }
}

/// Server statistics record.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ServerStats {
    pub id: i64,
    pub server_id: i64,
    pub latency_ms: Option<i64>,
    pub is_available: bool,
    pub checked_at: i64,
}

impl ServerStats {
    pub fn checked_at(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.checked_at, 0).unwrap_or_default()
    }
}

/// SQLite storage for server data.
pub struct ServerStore {
    pub(crate) pool: SqlitePool,
    pub(crate) config: StoreConfig,
}

impl ServerStore {
    /// Create a new store with default config.
    pub async fn new(path: impl Into<String>) -> Result<Self> {
        Self::with_config(StoreConfig {
            path: path.into(),
            ..Default::default()
        })
        .await
    }

    /// Create a new store with custom config.
    pub async fn with_config(config: StoreConfig) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", config.path))?
            .create_if_missing(true);

        let pool = SqlitePool::connect_with(options).await?;

        let store = Self { pool, config };
        store.init_schema().await?;

        if store.config.enable_wal {
            sqlx::query("PRAGMA journal_mode=WAL")
                .execute(&store.pool)
                .await?;
        }

        Ok(store)
    }

    async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                host TEXT NOT NULL,
                port INTEGER NOT NULL DEFAULT 7709,
                region TEXT,
                source TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_checked_at INTEGER,
                UNIQUE(host, port)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS server_stats (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL,
                latency_ms INTEGER,
                is_available BOOLEAN NOT NULL DEFAULT 1,
                checked_at INTEGER NOT NULL,
                FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_server_stats_checked_at ON server_stats(checked_at DESC)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_server_stats_server_id ON server_stats(server_id)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_servers_host ON servers(host)")
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Add or update a server.
    pub async fn upsert_server(
        &self,
        host: &str,
        port: u16,
        region: Option<&str>,
        source: &str,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();

        let id = sqlx::query_scalar(
            r#"
            INSERT INTO servers (host, port, region, source, created_at, last_checked_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(host, port) DO UPDATE SET
                region = ?3,
                source = ?4,
                last_checked_at = ?6
            RETURNING id
            "#,
        )
        .bind(host)
        .bind(port as i64)
        .bind(region)
        .bind(source)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        Ok(id)
    }

    /// Get server ID by host and port.
    pub async fn get_server_id(&self, host: &str, port: u16) -> Result<Option<i64>> {
        let result = sqlx::query_scalar("SELECT id FROM servers WHERE host = ?1 AND port = ?2")
            .bind(host)
            .bind(port as i64)
            .fetch_optional(&self.pool)
            .await?;

        Ok(result)
    }

    /// Record probe statistics for a server.
    pub async fn record_stats(
        &self,
        server_id: i64,
        latency_ms: Option<u64>,
        is_available: bool,
    ) -> Result<i64> {
        let now = Utc::now().timestamp();

        let id = sqlx::query_scalar(
            r#"
            INSERT INTO server_stats (server_id, latency_ms, is_available, checked_at)
            VALUES (?1, ?2, ?3, ?4)
            RETURNING id
            "#,
        )
        .bind(server_id)
        .bind(latency_ms.map(|l| l as i64))
        .bind(is_available)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        // Update last_checked_at on the server
        sqlx::query("UPDATE servers SET last_checked_at = ?1 WHERE id = ?2")
            .bind(now)
            .bind(server_id)
            .execute(&self.pool)
            .await?;

        Ok(id)
    }

    /// Get available servers ordered by latency.
    pub async fn get_available_servers(&self, limit: usize) -> Result<Vec<ServerRecord>> {
        let cutoff = Utc::now().timestamp() - 3600; // Last hour

        let servers = sqlx::query_as::<_, ServerRecord>(
            r#"
            SELECT s.*,
                   CAST(AVG(ss.latency_ms) AS REAL) as avg_latency,
                   CAST(MIN(ss.latency_ms) AS REAL) as min_latency,
                   COUNT(*) as check_count
            FROM servers s
            INNER JOIN server_stats ss ON s.id = ss.server_id
            WHERE ss.is_available = 1
              AND ss.checked_at > ?1
            GROUP BY s.id
            ORDER BY avg_latency ASC
            LIMIT ?2
            "#,
        )
        .bind(cutoff)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(servers)
    }

    /// Get the best server (lowest latency).
    pub async fn get_best_server(&self) -> Result<Option<ServerRecord>> {
        let servers = self.get_available_servers(1).await?;
        Ok(servers.into_iter().next())
    }

    /// Get all servers (including those without recent stats).
    pub async fn get_all_servers(&self) -> Result<Vec<ServerRecord>> {
        let servers = sqlx::query_as::<_, ServerRecord>(
            r#"
            SELECT s.*,
                   CAST(AVG(ss.latency_ms) AS REAL) as avg_latency,
                   CAST(MIN(ss.latency_ms) AS REAL) as min_latency,
                   COUNT(*) as check_count
            FROM servers s
            LEFT JOIN server_stats ss ON s.id = ss.server_id
            GROUP BY s.id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(servers)
    }

    /// Clean up old statistics data.
    pub async fn cleanup_old_stats(&self) -> Result<u64> {
        let cutoff = Utc::now().timestamp() - self.config.stats_retention.num_seconds();

        let result = sqlx::query("DELETE FROM server_stats WHERE checked_at < ?1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    /// Get recent statistics for a server.
    pub async fn get_server_stats(&self, server_id: i64, limit: usize) -> Result<Vec<ServerStats>> {
        let stats = sqlx::query_as::<_, ServerStats>(
            "SELECT * FROM server_stats WHERE server_id = ?1 ORDER BY checked_at DESC LIMIT ?2",
        )
        .bind(server_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_store_create() {
        let store = ServerStore::new(":memory:").await.unwrap();
        let servers = store.get_all_servers().await.unwrap();
        assert_eq!(servers.len(), 0);
    }

    #[tokio::test]
    async fn test_upsert_server() {
        let store = ServerStore::new(":memory:").await.unwrap();
        let id = store
            .upsert_server("127.0.0.1", 7709, None, "test")
            .await
            .unwrap();
        assert!(id > 0);

        // Upsert same server should return same ID
        let id2 = store
            .upsert_server("127.0.0.1", 7709, None, "test")
            .await
            .unwrap();
        assert_eq!(id, id2);
    }

    #[tokio::test]
    async fn test_record_and_get_stats() {
        let store = ServerStore::new(":memory:").await.unwrap();
        let server_id = store
            .upsert_server("127.0.0.1", 7709, None, "test")
            .await
            .unwrap();

        store.record_stats(server_id, Some(50), true).await.unwrap();
        store.record_stats(server_id, Some(60), true).await.unwrap();

        let stats = store.get_server_stats(server_id, 10).await.unwrap();
        assert_eq!(stats.len(), 2);
    }

    #[tokio::test]
    async fn test_get_best_server() {
        let store = ServerStore::new(":memory:").await.unwrap();
        let id1 = store
            .upsert_server("127.0.0.1", 7709, None, "test")
            .await
            .unwrap();
        let id2 = store
            .upsert_server("127.0.0.2", 7709, None, "test")
            .await
            .unwrap();

        // Record stats: id1 with latency 100, id2 with latency 50
        store.record_stats(id1, Some(100), true).await.unwrap();
        store.record_stats(id2, Some(50), true).await.unwrap();

        let best = store.get_best_server().await.unwrap();
        assert!(best.is_some());
        assert_eq!(best.unwrap().host, "127.0.0.2");
    }

    #[tokio::test]
    async fn test_get_available_servers() {
        let store = ServerStore::new(":memory:").await.unwrap();
        let id = store
            .upsert_server("127.0.0.1", 7709, None, "test")
            .await
            .unwrap();

        // Record available stat
        store.record_stats(id, Some(50), true).await.unwrap();

        let available = store.get_available_servers(10).await.unwrap();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].host, "127.0.0.1");
    }

    #[tokio::test]
    async fn test_get_server_id() {
        let store = ServerStore::new(":memory:").await.unwrap();
        let id = store
            .upsert_server("127.0.0.1", 7709, None, "test")
            .await
            .unwrap();

        let found = store.get_server_id("127.0.0.1", 7709).await.unwrap();
        assert_eq!(found, Some(id));

        let not_found = store.get_server_id("192.168.1.1", 7709).await.unwrap();
        assert_eq!(not_found, None);
    }

    #[tokio::test]
    async fn test_store_config_default() {
        let config = StoreConfig::default();
        assert!(config.enable_wal);
        assert_eq!(config.path, "rstdx_servers.db");
    }
}
