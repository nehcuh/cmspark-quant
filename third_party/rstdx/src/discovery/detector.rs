//! IP detector for probing TDX servers.

use crate::client::TdxConnection;
use crate::commands::setup::setup_connection;
use crate::discovery::source::ServerInfo;
use crate::discovery::store::ServerStore;
use crate::error::Result;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Detector configuration.
#[derive(Debug, Clone)]
pub struct DetectionConfig {
    /// Timeout for each probe attempt.
    pub timeout: Duration,
    /// Maximum concurrent probes.
    pub max_concurrent: usize,
    /// Minimum interval between probes for the same server.
    pub min_interval: Duration,
    /// Number of retries for failed probes.
    pub retry_count: usize,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
            max_concurrent: 50,
            min_interval: Duration::from_secs(30),
            retry_count: 2,
        }
    }
}

/// Result of probing a single server.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub host: String,
    pub port: u16,
    pub latency_ms: Option<u64>,
    pub is_available: bool,
    pub error: Option<String>,
    pub probed_at: DateTime<Utc>,
}

/// IP detector for discovering and testing TDX servers.
pub struct IpDetector {
    config: DetectionConfig,
    store: ServerStore,
    semaphore: Arc<Semaphore>,
}

impl IpDetector {
    /// Create a new detector with default config.
    pub async fn new(store: ServerStore) -> Result<Self> {
        Self::with_config(DetectionConfig::default(), store).await
    }

    /// Create a new detector with custom config.
    pub async fn with_config(config: DetectionConfig, store: ServerStore) -> Result<Self> {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent));
        Ok(Self {
            config,
            store,
            semaphore,
        })
    }

    /// Probe a single server.
    pub async fn probe_single(&self, server: ServerInfo) -> ProbeResult {
        Self::probe_server_static(server, self.config.timeout).await
    }

    /// Probe multiple servers concurrently.
    pub async fn probe(&self, servers: Vec<ServerInfo>) -> Vec<ProbeResult> {
        let semaphore = self.semaphore.clone();

        let tasks: Vec<_> = servers
            .into_iter()
            .map(|server| {
                let semaphore = semaphore.clone();
                let timeout = self.config.timeout;

                tokio::spawn(async move {
                    let _permit = semaphore.acquire().await.unwrap();
                    Self::probe_server_static(server, timeout).await
                })
            })
            .collect();

        let mut results = Vec::new();
        for task in tasks {
            if let Ok(result) = task.await {
                results.push(result);
            }
        }

        // Sort by latency (available servers first, then by latency)
        results.sort_by(|a, b| match (a.is_available, b.is_available) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a
                .latency_ms
                .unwrap_or(u64::MAX)
                .cmp(&b.latency_ms.unwrap_or(u64::MAX)),
        });

        results
    }

    /// Static version of probe_single for use in spawned tasks.
    async fn probe_server_static(server: ServerInfo, timeout: Duration) -> ProbeResult {
        let start = Instant::now();

        let result = tokio::time::timeout(timeout, async {
            let addr = server.addr();
            let mut conn = TdxConnection::connect(&addr).await?;

            // Perform proper TDX handshake
            setup_connection(&mut conn).await?;

            Ok::<(), crate::error::Error>(())
        })
        .await;

        let latency = start.elapsed();

        match result {
            Ok(Ok(())) => ProbeResult {
                host: server.host,
                port: server.port,
                latency_ms: Some(latency.as_millis() as u64),
                is_available: true,
                error: None,
                probed_at: Utc::now(),
            },
            Ok(Err(e)) => ProbeResult {
                host: server.host,
                port: server.port,
                latency_ms: None,
                is_available: false,
                error: Some(e.to_string()),
                probed_at: Utc::now(),
            },
            Err(_) => ProbeResult {
                host: server.host,
                port: server.port,
                latency_ms: None,
                is_available: false,
                error: Some("Timeout".to_string()),
                probed_at: Utc::now(),
            },
        }
    }

    /// Probe servers and persist results to store.
    pub async fn probe_and_store(&self, servers: Vec<ServerInfo>) -> Result<Vec<ProbeResult>> {
        let results = self.probe(servers).await;

        for result in &results {
            if let Ok(Some(server_id)) = self.store.get_server_id(&result.host, result.port).await {
                self.store
                    .record_stats(server_id, result.latency_ms, result.is_available)
                    .await
                    .ok();
            } else {
                // Server not in store yet, add it
                if let Ok(server_id) = self
                    .store
                    .upsert_server(&result.host, result.port, None, "discovered")
                    .await
                {
                    self.store
                        .record_stats(server_id, result.latency_ms, result.is_available)
                        .await
                        .ok();
                }
            }
        }

        Ok(results)
    }

    /// Get the best server from the store.
    pub async fn best_server(&self) -> Option<crate::discovery::store::ServerRecord> {
        self.store.get_best_server().await.ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_probe_single() {
        let store = ServerStore::new(":memory:").await.unwrap();
        let detector = IpDetector::new(store).await.unwrap();

        // Probe localhost (expected to fail, but tests the logic)
        let server = ServerInfo {
            host: "127.0.0.1".to_string(),
            port: 7709,
            region: None,
            source: "test".to_string(),
        };

        let result = detector.probe_single(server).await;
        assert!(!result.is_available); // No server running on localhost:7709
    }

    #[tokio::test]
    async fn test_probe_multiple() {
        let store = ServerStore::new(":memory:").await.unwrap();
        let detector = IpDetector::new(store).await.unwrap();

        let servers = vec![
            ServerInfo {
                host: "127.0.0.1".to_string(),
                port: 7709,
                region: None,
                source: "test".to_string(),
            },
            ServerInfo {
                host: "127.0.0.2".to_string(),
                port: 7709,
                region: None,
                source: "test".to_string(),
            },
        ];

        let results = detector.probe(servers).await;
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_detection_config_default() {
        let config = DetectionConfig::default();
        assert_eq!(config.max_concurrent, 50);
        assert_eq!(config.retry_count, 2);
    }

    #[test]
    fn test_probe_result_available() {
        let result = ProbeResult {
            host: "127.0.0.1".to_string(),
            port: 7709,
            latency_ms: Some(50),
            is_available: true,
            error: None,
            probed_at: chrono::Utc::now(),
        };
        assert!(result.is_available);
        assert_eq!(result.latency_ms, Some(50));
    }

    #[test]
    fn test_probe_result_unavailable() {
        let result = ProbeResult {
            host: "127.0.0.1".to_string(),
            port: 7709,
            latency_ms: None,
            is_available: false,
            error: Some("Timeout".to_string()),
            probed_at: chrono::Utc::now(),
        };
        assert!(!result.is_available);
        assert!(result.error.is_some());
    }
}
