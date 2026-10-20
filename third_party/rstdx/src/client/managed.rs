//! Managed client with automatic reconnection and heartbeat.

use crate::client::TdxConnection;
use crate::client::{HeartbeatConfig, HeartbeatManager, ReconnectConfig, ReconnectManager};
use crate::commands::{self, Kline, MinuteTimeData, SecurityItem, SecurityQuote, TransactionData};
use crate::discovery::source::DEFAULT_TDX_SERVERS;
use crate::error::{Error, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Execute an operation with automatic reconnect on recoverable errors.
macro_rules! managed_op {
    ($self:ident, |$conn:ident| $body:expr) => {{
        loop {
            let result = {
                let mut __guard = $self.inner.conn.lock().await;
                let $conn = &mut *__guard;
                $body
            };
            match result {
                Ok(r) => break Ok(r),
                Err(e) => {
                    if !matches!(e, Error::Io(_) | Error::Connection(_) | Error::Protocol(_)) {
                        break Err(e);
                    }
                    warn!("Operation failed, attempting reconnect: {}", e);
                    $self.inner.reconnect().await?;
                }
            }
        }
    }};
}

/// Managed connection that wraps a TDX connection with auto-reconnect.
///
/// The connection is shared via `Arc<Mutex<_>>` so that the heartbeat
/// task and the client can both access (and replace) it.
#[derive(Debug)]
struct ManagedConnection {
    conn: Arc<Mutex<TdxConnection>>,
    reconnect: ReconnectManager,
    addr: String,
}

impl ManagedConnection {
    /// Create a new managed connection with custom reconnect config.
    async fn connect_with_config(addr: &str, config: ReconnectConfig) -> Result<Self> {
        let conn = Self::connect_to_server(addr).await?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            reconnect: ReconnectManager::with_config(config),
            addr: addr.to_string(),
        })
    }

    /// Helper to connect and perform handshake.
    async fn connect_to_server(addr: &str) -> Result<TdxConnection> {
        let mut conn = TdxConnection::connect(addr).await?;
        commands::setup_connection(&mut conn).await?;
        Ok(conn)
    }

    /// Force a reconnection.
    async fn reconnect(&mut self) -> Result<()> {
        let addr = self.addr.clone();
        let new_conn = self
            .reconnect
            .reconnect_with_backoff(|| Self::connect_to_server(&addr))
            .await?;

        let mut guard = self.conn.lock().await;
        *guard = new_conn;
        info!("Reconnected to {}", self.addr);
        Ok(())
    }

    /// Get a clone of the shared connection handle.
    fn conn_handle(&self) -> Arc<Mutex<TdxConnection>> {
        Arc::clone(&self.conn)
    }
}

/// High-level TDX client with automatic reconnection and heartbeat.
///
/// Provides the same API as [`TdxClient`](crate::client::TdxClient) but will automatically
/// reconnect on network failures and send periodic heartbeats to
/// keep the connection alive.
///
/// # Example
/// ```no_run
/// # use rstdx::client::ManagedTdxClient;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut client = ManagedTdxClient::connect("119.147.212.81:7709").await?;
/// let quotes = client.get_security_quotes(&[(1, "600000")]).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ManagedTdxClient {
    inner: ManagedConnection,
    heartbeat: HeartbeatManager,
}

impl ManagedTdxClient {
    /// Connect to a TDX server with managed connection.
    pub async fn connect(addr: &str) -> Result<Self> {
        Self::connect_with_config(addr, ReconnectConfig::default(), HeartbeatConfig::default())
            .await
    }

    /// Connect with custom reconnection and heartbeat configuration.
    pub async fn connect_with_config(
        addr: &str,
        reconnect_config: ReconnectConfig,
        heartbeat_config: HeartbeatConfig,
    ) -> Result<Self> {
        let inner = ManagedConnection::connect_with_config(addr, reconnect_config).await?;
        let heartbeat = HeartbeatManager::with_config(heartbeat_config);
        heartbeat.start(inner.conn_handle()).await?;

        Ok(Self { inner, heartbeat })
    }

    /// Automatically connect to the best available TDX server with management.
    pub async fn connect_auto() -> Result<Self> {
        Self::connect_auto_with_config(ReconnectConfig::default(), HeartbeatConfig::default()).await
    }

    /// Automatically connect with custom configuration.
    pub async fn connect_auto_with_config(
        reconnect_config: ReconnectConfig,
        heartbeat_config: HeartbeatConfig,
    ) -> Result<Self> {
        for (host, port) in DEFAULT_TDX_SERVERS {
            let addr = format!("{}:{}", host, port);
            debug!("Trying managed auto-connect to: {}", addr);

            match tokio::time::timeout(
                Duration::from_secs(2),
                Self::connect_with_config(
                    &addr,
                    reconnect_config.clone(),
                    heartbeat_config.clone(),
                ),
            )
            .await
            {
                Ok(Ok(client)) => {
                    debug!("Managed auto-connected to: {}", addr);
                    return Ok(client);
                }
                Ok(Err(e)) => {
                    debug!("Failed to connect to {}: {}", addr, e);
                }
                Err(_) => {
                    debug!("Timeout connecting to: {}", addr);
                }
            }
        }

        Err(Error::NoAvailableServer)
    }

    /// Get K-line data.
    pub async fn get_security_bars(
        &mut self,
        category: u16,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<Kline>> {
        let mut code_bytes = [0u8; 6];
        let code_len = code.len().min(6);
        code_bytes[..code_len].copy_from_slice(code.as_bytes());

        managed_op!(self, |conn| {
            commands::get_security_bars(conn, category, market, &code_bytes, start, count).await
        })
    }

    /// Get security quotes for multiple stocks.
    pub async fn get_security_quotes(
        &mut self,
        stocks: &[(u8, &str)],
    ) -> Result<Vec<SecurityQuote>> {
        let mut code_array: Vec<[u8; 6]> = Vec::with_capacity(stocks.len());
        let mut stock_refs: Vec<(u8, &[u8; 6])> = Vec::with_capacity(stocks.len());

        for (_market, code) in stocks {
            let mut code_bytes = [0u8; 6];
            let code_len = code.len().min(6);
            code_bytes[..code_len].copy_from_slice(code.as_bytes());
            code_array.push(code_bytes);
        }

        for (i, (market, _)) in stocks.iter().enumerate() {
            stock_refs.push((*market, &code_array[i]));
        }

        managed_op!(self, |conn| {
            commands::get_security_quotes(conn, &stock_refs).await
        })
    }

    /// Get security list for a market.
    pub async fn get_security_list(&mut self, market: u8, start: u16) -> Result<Vec<SecurityItem>> {
        managed_op!(self, |conn| {
            commands::get_security_list(conn, market, start).await
        })
    }

    /// Get financial information for a stock.
    pub async fn get_finance_info(
        &mut self,
        market: u8,
        code: &str,
    ) -> Result<commands::FinanceInfo> {
        let mut code_bytes = [0u8; 6];
        let code_len = code.len().min(6);
        code_bytes[..code_len].copy_from_slice(code.as_bytes());

        managed_op!(self, |conn| {
            commands::get_finance_info(conn, market, &code_bytes).await
        })
    }

    /// Get transaction (tick-by-tick) data for a security.
    pub async fn get_transaction_data(
        &mut self,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<TransactionData>> {
        let mut code_bytes = [0u8; 6];
        let code_len = code.len().min(6);
        code_bytes[..code_len].copy_from_slice(code.as_bytes());

        managed_op!(self, |conn| {
            commands::get_transaction_data(conn, market, &code_bytes, start, count).await
        })
    }

    /// Get minute time-sharing data for a security (today's intraday data).
    pub async fn get_minute_time_data(
        &mut self,
        market: u8,
        code: &str,
    ) -> Result<Vec<MinuteTimeData>> {
        let mut code_bytes = [0u8; 6];
        let code_len = code.len().min(6);
        code_bytes[..code_len].copy_from_slice(code.as_bytes());

        managed_op!(self, |conn| {
            commands::get_minute_time_data(conn, market, &code_bytes).await
        })
    }

    /// Stop the heartbeat task.
    pub async fn stop_heartbeat(&self) {
        self.heartbeat.stop().await;
    }

    /// Check if heartbeat is running.
    pub async fn is_heartbeat_running(&self) -> bool {
        self.heartbeat.is_running().await
    }

    /// Close the connection gracefully.
    pub async fn close(&mut self) -> Result<()> {
        self.stop_heartbeat().await;
        let mut conn = self.inner.conn.lock().await;
        conn.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconnect_config_default_clone() {
        let config = ReconnectConfig::default();
        let _cloned = config.clone();
    }

    #[test]
    fn test_heartbeat_config_default_clone() {
        let config = HeartbeatConfig::default();
        let _cloned = config.clone();
    }
}
