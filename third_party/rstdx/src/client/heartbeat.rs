//! Heartbeat management for keeping TDX connections alive.

use crate::client::TdxConnection;
use crate::error::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{Instant, interval};
use tracing::{debug, warn};

/// Heartbeat configuration.
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    /// Interval between heartbeats.
    pub interval: Duration,
    /// Timeout for each heartbeat request.
    pub timeout: Duration,
    /// Whether to enable heartbeat.
    pub enabled: bool,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(5),
            enabled: true,
        }
    }
}

/// Heartbeat manager for TDX connections.
///
/// Sends periodic heartbeat requests to keep the connection alive.
/// The heartbeat is a simple stock count query (random market).
#[derive(Debug)]
pub struct HeartbeatManager {
    config: HeartbeatConfig,
    shutdown: Arc<AtomicBool>,
    task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Default for HeartbeatManager {
    fn default() -> Self {
        Self::new()
    }
}

impl HeartbeatManager {
    /// Create a new heartbeat manager with default config.
    pub fn new() -> Self {
        Self::with_config(HeartbeatConfig::default())
    }

    /// Create a new heartbeat manager with custom config.
    pub fn with_config(config: HeartbeatConfig) -> Self {
        Self {
            config,
            shutdown: Arc::new(AtomicBool::new(false)),
            task_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Start the heartbeat task.
    ///
    /// The task will send periodic heartbeat requests to keep the connection alive.
    pub async fn start(&self, conn: Arc<Mutex<TdxConnection>>) -> Result<()> {
        if !self.config.enabled {
            debug!("Heartbeat is disabled, not starting task");
            return Ok(());
        }

        let shutdown = Arc::clone(&self.shutdown);
        let heartbeat_interval = self.config.interval;
        let timeout = self.config.timeout;

        let handle = tokio::spawn(async move {
            let mut ticker = interval(heartbeat_interval);
            ticker.tick().await; // Skip first immediate tick

            loop {
                ticker.tick().await;

                if shutdown.load(Ordering::Relaxed) {
                    debug!("Heartbeat task received shutdown signal");
                    break;
                }

                // Send heartbeat
                let start = Instant::now();
                match tokio::time::timeout(timeout, Self::send_heartbeat(Arc::clone(&conn))).await {
                    Ok(Ok(())) => {
                        debug!("Heartbeat sent successfully in {:?}", start.elapsed());
                    }
                    Ok(Err(e)) => {
                        warn!("Heartbeat failed: {}", e);
                    }
                    Err(_) => {
                        warn!("Heartbeat timed out after {:?}", timeout);
                    }
                }
            }
        });

        let mut task = self.task_handle.lock().await;
        *task = Some(handle);

        debug!(
            "Heartbeat task started with interval {:?}",
            self.config.interval
        );
        Ok(())
    }

    /// Stop the heartbeat task.
    pub async fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);

        let mut task = self.task_handle.lock().await;
        if let Some(handle) = task.take() {
            // Give the task a moment to shut down gracefully
            let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
            debug!("Heartbeat task stopped");
        }
    }

    /// Send a heartbeat request.
    ///
    /// This is a simple stock count query which is lightweight.
    async fn send_heartbeat(conn: Arc<Mutex<TdxConnection>>) -> Result<()> {
        let mut conn_guard = conn.lock().await;

        // Send a simple heartbeat packet (market count query)
        // Format: <HIHHHHB (similar to setup commands)
        let heartbeat_pkg = [
            0x0c, 0x00, // magic
            0x01, 0x64, 0x01, 0x00, // cmd
            0x08, 0x00, // len1
            0x08, 0x00, // len2
            0x00, 0x3e, // cmd2
            0x00, // market (Shanghai)
        ];

        conn_guard.send(&heartbeat_pkg).await?;

        // We don't care about the response, just need to keep connection alive
        let _ = conn_guard.recv_response().await;

        Ok(())
    }

    /// Check if the heartbeat task is running.
    pub async fn is_running(&self) -> bool {
        let task = self.task_handle.lock().await;
        task.is_some()
    }
}

impl Drop for HeartbeatManager {
    fn drop(&mut self) {
        // Ensure shutdown flag is set
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Heartbeat-enabled client wrapper.
///
/// Wraps a TdxConnection with automatic heartbeat management.
pub struct HeartbeatClient {
    conn: Arc<Mutex<TdxConnection>>,
    heartbeat: HeartbeatManager,
}

impl HeartbeatClient {
    /// Create a new heartbeat client.
    pub async fn connect(addr: &str) -> Result<Self> {
        Self::connect_with_config(addr, HeartbeatConfig::default()).await
    }

    /// Create a new heartbeat client with custom config.
    pub async fn connect_with_config(addr: &str, config: HeartbeatConfig) -> Result<Self> {
        let mut conn = TdxConnection::connect(addr).await?;
        crate::commands::setup_connection(&mut conn).await?;

        let conn = Arc::new(Mutex::new(conn));
        let heartbeat = HeartbeatManager::with_config(config.clone());
        heartbeat.start(Arc::clone(&conn)).await?;

        Ok(Self { conn, heartbeat })
    }

    /// Get the underlying connection.
    pub fn conn(&self) -> Arc<Mutex<TdxConnection>> {
        Arc::clone(&self.conn)
    }

    /// Stop the heartbeat task.
    pub async fn stop_heartbeat(&self) {
        self.heartbeat.stop().await;
    }

    /// Check if heartbeat is running.
    pub async fn is_heartbeat_running(&self) -> bool {
        self.heartbeat.is_running().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heartbeat_config_default() {
        let config = HeartbeatConfig::default();
        assert_eq!(config.interval, Duration::from_secs(10));
        assert_eq!(config.timeout, Duration::from_secs(5));
        assert!(config.enabled);
    }

    #[test]
    fn test_heartbeat_manager_disabled() {
        let config = HeartbeatConfig {
            enabled: false,
            ..Default::default()
        };
        let manager = HeartbeatManager::with_config(config);
        assert!(!manager.config.enabled);
    }

    #[tokio::test]
    async fn test_heartbeat_manager_start_stop() {
        let manager = HeartbeatManager::new();
        assert!(!manager.is_running().await);

        // Note: We can't actually start it without a real connection
        // This just verifies the state management
        manager.stop().await;
        assert!(!manager.is_running().await);
    }

    #[test]
    fn test_heartbeat_config_custom() {
        let config = HeartbeatConfig {
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(3),
            enabled: false,
        };
        assert_eq!(config.interval, Duration::from_secs(5));
        assert_eq!(config.timeout, Duration::from_secs(3));
        assert!(!config.enabled);
    }

    #[test]
    fn test_heartbeat_manager_new() {
        let manager = HeartbeatManager::new();
        assert!(manager.config.enabled);
    }
}
