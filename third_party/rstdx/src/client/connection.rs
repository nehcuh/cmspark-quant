//! TCP connection for TDX protocol.

use crate::commands::{self, Kline, MinuteTimeData, SecurityItem, SecurityQuote, TransactionData};
use crate::discovery::source::DEFAULT_TDX_SERVERS;
use crate::error::{Error, Result};
use crate::protocol::ResponseHeader;
use async_trait::async_trait;
use bytes::BytesMut;
use flate2::read::ZlibDecoder;
use std::io::Read;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, trace};

/// Response header size (16 bytes).
const RESPONSE_HEADER_LEN: usize = 0x10;

/// Trait abstracting TDX protocol connections.
///
/// This trait enables mocking in tests and allows wrapping connections
/// with reconnection / heartbeat logic without changing command code.
#[async_trait]
pub trait Connection: Send {
    /// Send a request package.
    async fn send(&mut self, data: &[u8]) -> Result<()>;

    /// Receive a full response (header + body).
    async fn recv_response(&mut self) -> Result<Vec<u8>>;

    /// Best-effort check whether the connection is still alive.
    fn is_connected(&self) -> bool {
        true // Default: assume alive (overridden by TdxConnection)
    }
}

/// TDX TCP connection.
#[derive(Debug)]
pub struct TdxConnection {
    stream: TcpStream,
    read_buf: BytesMut,
}

impl TdxConnection {
    /// Connect to a TDX server.
    pub async fn connect(addr: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr).await.map_err(|e| {
            Error::Connection(format!("Failed to connect to {}: {}", addr, e))
        })?;
        stream.set_nodelay(true)?;
        debug!("Connected to TDX server: {}", addr);
        Ok(Self {
            stream,
            read_buf: BytesMut::with_capacity(8192),
        })
    }

    /// Gracefully shutdown the TCP connection.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }

    /// Send a request package.
    pub async fn send(&mut self, pkg: &[u8]) -> Result<()> {
        trace!("Sending request: {} bytes", pkg.len());
        self.stream.write_all(pkg).await?;
        Ok(())
    }

    /// Receive response header.
    async fn recv_header(&mut self) -> Result<ResponseHeader> {
        self.read_buf.clear();
        self.read_buf.resize(RESPONSE_HEADER_LEN, 0);

        self.stream.read_exact(&mut self.read_buf).await?;

        // Parse header via protocol module
        let header = crate::protocol::header::parse_response_header(&self.read_buf[..])
            .ok_or_else(|| Error::InvalidResponse("Failed to parse response header".to_string()))?;

        debug!(
            "Response header: zip={}, unzip={}",
            header.zip_size, header.unzip_size
        );

        Ok(header)
    }

    /// Receive response body.
    async fn recv_body(&mut self, header: &ResponseHeader) -> Result<Vec<u8>> {
        self.read_buf.clear();
        self.read_buf.resize(header.zip_size as usize, 0);

        // No timeout for recv - match pytdx behavior which uses settimeout(None) after connect
        // The server may send data very slowly (~700 bytes/sec), so we need patience
        match self.stream.read_exact(&mut self.read_buf[..]).await {
            Ok(_) => {}
            Err(e) => return Err(Error::Connection(format!("Read error: {}", e))),
        }

        let body = if header.zip_size == header.unzip_size {
            debug!("Response body not compressed");
            self.read_buf[..].to_vec()
        } else {
            debug!(
                "Decompressing response body ({} -> {} bytes)",
                header.zip_size, header.unzip_size
            );
            decompress(&self.read_buf[..], header.unzip_size as usize)?
        };

        Ok(body)
    }

    /// Receive full response (header + body).
    pub async fn recv_response(&mut self) -> Result<Vec<u8>> {
        let header = self.recv_header().await?;
        self.recv_body(&header).await
    }

    /// Quick check whether the underlying TCP stream is still connected.
    ///
    /// This is a best-effort check using `peer_addr()`; it can return
    /// `true` for half-open connections that will fail on the next write.
    /// Use it as a lightweight filter before returning a pooled connection.
    pub fn is_connected(&self) -> bool {
        self.stream.peer_addr().is_ok()
    }

    /// Create a test connection from an existing TCP stream.
    #[cfg(test)]
    pub(crate) fn from_stream(stream: TcpStream) -> Self {
        Self {
            stream,
            read_buf: BytesMut::new(),
        }
    }
}

#[async_trait]
impl Connection for TdxConnection {
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        TdxConnection::send(self, data).await
    }

    async fn recv_response(&mut self) -> Result<Vec<u8>> {
        TdxConnection::recv_response(self).await
    }
}

/// Mock connection for testing command logic without network I/O.
///
/// Pre-configured with a sequence of responses that are returned
/// in order for each `recv_response` call.
#[derive(Debug, Clone)]
pub enum MockResponse {
    /// Successful response body.
    Ok(Vec<u8>),
    /// Simulated error.
    Err(String),
}

/// Mock connection for testing.
#[derive(Debug)]
pub struct MockConnection {
    responses: Vec<MockResponse>,
    sent: Vec<Vec<u8>>,
    index: usize,
}

impl MockConnection {
    /// Create a new mock connection with the given response sequence.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses,
            sent: Vec::new(),
            index: 0,
        }
    }

    /// Get all data that was sent through this connection.
    pub fn sent_data(&self) -> &[Vec<u8>] {
        &self.sent
    }
}

#[async_trait]
impl Connection for MockConnection {
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        self.sent.push(data.to_vec());
        Ok(())
    }

    async fn recv_response(&mut self) -> Result<Vec<u8>> {
        if self.index >= self.responses.len() {
            return Err(Error::Connection(
                "MockConnection: no more responses".to_string(),
            ));
        }
        let result = match &self.responses[self.index] {
            MockResponse::Ok(data) => Ok(data.clone()),
            MockResponse::Err(msg) => Err(Error::Connection(msg.clone())),
        };
        self.index += 1;
        result
    }
}

/// Decompress zlib data.
fn decompress(data: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut decompressed = Vec::with_capacity(expected_len);
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| Error::Compression(format!("Decompression failed: {}", e)))?;
    Ok(decompressed)
}

/// High-level TDX client.
#[derive(Debug)]
pub struct TdxClient {
    conn: TdxConnection,
}

impl TdxClient {
    /// Create a client from an already-established (and handshaked) connection.
    pub fn from_connection(conn: TdxConnection) -> Self {
        Self { conn }
    }

    /// Connect to a TDX server and perform handshake.
    ///
    /// # Example
    /// ```no_run
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = rstdx::client::TdxClient::connect("114.80.63.12:7709").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect(addr: &str) -> Result<Self> {
        let mut conn = TdxConnection::connect(addr).await?;
        commands::setup_connection(&mut conn).await?;
        Ok(Self { conn })
    }

    /// Automatically connect to the best available TDX server.
    /// This will try servers from the default list and select the first one that responds.
    pub async fn connect_auto() -> Result<Self> {
        // Try default servers in order
        for (host, port) in DEFAULT_TDX_SERVERS {
            let addr = format!("{}:{}", host, port);
            debug!("Trying auto-connect to: {}", addr);

            // Try to connect with a timeout
            match tokio::time::timeout(Duration::from_secs(2), Self::connect(&addr)).await {
                Ok(Ok(client)) => {
                    debug!("Auto-connected to: {}", addr);
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

        commands::get_security_bars(&mut self.conn, category, market, &code_bytes, start, count)
            .await
    }

    /// Get security quotes for multiple stocks.
    ///
    /// # Arguments
    /// * `stocks` - Slice of (market, code) tuples
    ///   - market: 0 for Shenzhen, 1 for Shanghai
    ///   - code: Stock code (e.g., "000001", "600000")
    ///
    /// # Example
    /// ```no_run
    /// # use rstdx::client::TdxClient;
    /// # async fn example(client: &mut TdxClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let quotes = client.get_security_quotes(&[(0, "000001"), (1, "600000")]).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_security_quotes(
        &mut self,
        stocks: &[(u8, &str)],
    ) -> Result<Vec<SecurityQuote>> {
        // Create array of code references
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

        commands::get_security_quotes(&mut self.conn, &stock_refs).await
    }

    /// Get security list for a market.
    ///
    /// # Arguments
    /// * `market` - Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
    /// * `start` - Start position for pagination (0-1000 recommended)
    ///
    /// # Example
    /// ```no_run
    /// # use rstdx::client::TdxClient;
    /// # async fn example(client: &mut TdxClient) -> Result<(), Box<dyn std::error::Error>> {
    /// // Get first 1000 stocks from Shanghai market
    /// let stocks = client.get_security_list(1, 0).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_security_list(&mut self, market: u8, start: u16) -> Result<Vec<SecurityItem>> {
        commands::get_security_list(&mut self.conn, market, start).await
    }

    /// Get financial information for a stock.
    ///
    /// # Arguments
    /// * `market` - Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
    /// * `code` - Stock code (e.g., "000001", "600000")
    ///
    /// # Returns
    /// Financial information including equity structure, assets, liabilities, and income statement
    ///
    /// # Example
    /// ```no_run
    /// # use rstdx::client::TdxClient;
    /// # async fn example(client: &mut TdxClient) -> Result<(), Box<dyn std::error::Error>> {
    /// let info = client.get_finance_info(1, "600000").await?;
    /// println!("Total assets: {}, Net profit: {}", info.total_assets, info.net_profit);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_finance_info(
        &mut self,
        market: u8,
        code: &str,
    ) -> Result<commands::FinanceInfo> {
        let mut code_bytes = [0u8; 6];
        let code_len = code.len().min(6);
        code_bytes[..code_len].copy_from_slice(code.as_bytes());

        commands::get_finance_info(&mut self.conn, market, &code_bytes).await
    }

    /// Get transaction (tick-by-tick) data for a security.
    ///
    /// # Arguments
    /// * `market` - Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
    /// * `code` - Stock code (e.g., "000001", "600000")
    /// * `start` - Start position (0 for latest)
    /// * `count` - Number of transactions to retrieve (max ~1800)
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

        commands::get_transaction_data(&mut self.conn, market, &code_bytes, start, count).await
    }

    /// Get minute time-sharing data for a security (today's intraday data).
    ///
    /// # Arguments
    /// * `market` - Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
    /// * `code` - Stock code (e.g., "000001", "600000")
    pub async fn get_minute_time_data(
        &mut self,
        market: u8,
        code: &str,
    ) -> Result<Vec<MinuteTimeData>> {
        let mut code_bytes = [0u8; 6];
        let code_len = code.len().min(6);
        code_bytes[..code_len].copy_from_slice(code.as_bytes());

        commands::get_minute_time_data(&mut self.conn, market, &code_bytes).await
    }

    /// Get connection reference.
    pub fn conn(&mut self) -> &mut TdxConnection {
        &mut self.conn
    }

    /// Close the connection gracefully.
    pub async fn close(&mut self) -> Result<()> {
        self.conn.shutdown().await
    }

    /// Consume the client and return the underlying connection.
    pub fn into_connection(self) -> TdxConnection {
        self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_header_from_protocol() {
        let header = ResponseHeader {
            zip_size: 100,
            unzip_size: 200,
        };
        assert_eq!(header.zip_size, 100);
        assert_eq!(header.unzip_size, 200);
    }

    #[test]
    fn test_response_header_copy() {
        let header = ResponseHeader {
            zip_size: 100,
            unzip_size: 200,
        };
        let copied = header;
        assert_eq!(copied.zip_size, 100);
    }

    #[test]
    fn test_mock_connection_basic() {
        let responses = vec![
            MockResponse::Ok(vec![0x01, 0x02, 0x03]),
            MockResponse::Ok(vec![0x04, 0x05]),
        ];
        let mut conn = MockConnection::new(responses);

        // Test send captures data
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            conn.send(b"hello").await.unwrap();
            assert_eq!(conn.sent_data(), vec![b"hello".to_vec()]);

            // Test recv_response returns responses in order
            let r1 = conn.recv_response().await.unwrap();
            assert_eq!(r1, vec![0x01, 0x02, 0x03]);

            let r2 = conn.recv_response().await.unwrap();
            assert_eq!(r2, vec![0x04, 0x05]);
        });
    }

    #[test]
    fn test_mock_connection_exhausted() {
        let mut conn = MockConnection::new(vec![]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = conn.recv_response().await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn test_mock_connection_error_response() {
        let responses = vec![MockResponse::Err("test error".to_string())];
        let mut conn = MockConnection::new(responses);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = conn.recv_response().await;
            assert!(result.is_err());
        });
    }
}
