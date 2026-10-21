//! GetTransactionData command - tick-by-tick transaction data.

use crate::client::TdxConnection;
use crate::codec::decode_price;
use crate::error::Result;
use crate::protocol::request;
use bytes::Buf;

/// Transaction direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionDirection {
    /// Buy transaction
    Buy,
    /// Sell transaction
    Sell,
    /// Neutral (unknown or opening)
    Neutral,
}

impl TransactionDirection {
    /// Create from raw byte value.
    pub fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::Buy,
            2 => Self::Sell,
            _ => Self::Neutral,
        }
    }

    /// Get human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Buy => "买",
            Self::Sell => "卖",
            Self::Neutral => "中性",
        }
    }
}

/// Individual transaction data point.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionData {
    /// Transaction time (HHMM format as u16)
    pub time: u16,
    /// Transaction price
    pub price: f64,
    /// Transaction volume
    pub volume: f64,
    /// Transaction direction (Buy/Sell/Neutral)
    pub direction: TransactionDirection,
}

impl TransactionData {
    /// Format time as HH:MM string.
    pub fn time_str(&self) -> String {
        let hour = self.time / 100;
        let minute = self.time % 100;
        format!("{:02}:{:02}", hour, minute)
    }
}

/// Get transaction data for a security.
///
/// # Arguments
/// * `conn` - TDX connection
/// * `market` - Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
/// * `code` - Stock code (6 bytes)
/// * `start` - Start position (0 for latest)
/// * `count` - Number of transactions to retrieve (max ~1800 for a full day)
///
/// # Returns
/// List of transaction data points, from most recent to oldest
pub async fn get_transaction_data(
    conn: &mut TdxConnection,
    market: u8,
    code: &[u8; 6],
    start: u16,
    count: u16,
) -> Result<Vec<TransactionData>> {
    let pkg = request::build_get_transaction_data(market, code, start, count)?;
    conn.send(&pkg).await?;

    let body = conn.recv_response().await?;
    parse_transaction_data(&body)
}

/// Parse transaction data response body.
fn parse_transaction_data(body: &[u8]) -> Result<Vec<TransactionData>> {
    let mut buf = body;

    // Skip first 2 bytes (unknown) + count (1 byte)
    ensure_bytes!(buf, 3);
    buf.advance(2);

    // Count of transactions (1 byte)
    let count = buf.get_u8();
    let mut transactions = Vec::with_capacity(count as usize);

    // Parse each transaction
    for _ in 0..count {
        // Create a temporary mutable reference for parsing
        let mut temp_buf = buf;
        let tx = parse_transaction(&mut temp_buf)?;
        transactions.push(tx);
        buf = temp_buf;
    }

    Ok(transactions)
}

/// Parse a single transaction from the buffer.
fn parse_transaction(buf: &mut &[u8]) -> Result<TransactionData> {
    // Minimum: time(2) + price(min 1) + vol(1) + direction(1) = 5 bytes
    ensure_bytes!(*buf, 5);

    // Time (2 bytes): HHMM format
    let time_raw = buf.get_u16_le();
    let time = (time_raw >> 8) * 100 + (time_raw & 0xFF);

    // Price (variable-length encoded)
    let (price_raw, price_pos) = decode_price(buf, 0)?;
    *buf = &buf[price_pos..];

    // Volume (1 byte, encoded)
    let vol_raw = buf.get_u8();
    let volume = decode_transaction_volume(vol_raw);

    // Direction (1 byte): 0=Neutral, 1=Buy, 2=Sell
    let direction_raw = buf.get_u8();
    let direction = TransactionDirection::from_raw(direction_raw);

    // Calculate actual price
    let price = price_raw as f64 / 100.0;

    Ok(TransactionData {
        time,
        price,
        volume,
        direction,
    })
}

/// Decode transaction volume from raw byte.
///
/// Transaction volume uses a simpler encoding than K-line volume.
fn decode_transaction_volume(raw: u8) -> f64 {
    // Each unit represents 100 shares (1 lot)
    // The encoding is: raw * 100
    // But some values have special meanings
    match raw {
        0 => 0.0,
        n => (n as f64) * 100.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_direction_from_raw() {
        assert_eq!(
            TransactionDirection::from_raw(0),
            TransactionDirection::Neutral
        );
        assert_eq!(TransactionDirection::from_raw(1), TransactionDirection::Buy);
        assert_eq!(
            TransactionDirection::from_raw(2),
            TransactionDirection::Sell
        );
        assert_eq!(
            TransactionDirection::from_raw(255),
            TransactionDirection::Neutral
        );
    }

    #[test]
    fn test_transaction_direction_name() {
        assert_eq!(TransactionDirection::Buy.name(), "买");
        assert_eq!(TransactionDirection::Sell.name(), "卖");
        assert_eq!(TransactionDirection::Neutral.name(), "中性");
    }

    #[test]
    fn test_transaction_time_str() {
        let tx = TransactionData {
            time: 930,
            price: 10.5,
            volume: 100.0,
            direction: TransactionDirection::Buy,
        };
        assert_eq!(tx.time_str(), "09:30");

        let tx2 = TransactionData { time: 1459, ..tx };
        assert_eq!(tx2.time_str(), "14:59");
    }

    #[test]
    fn test_decode_transaction_volume() {
        assert_eq!(decode_transaction_volume(0), 0.0);
        assert_eq!(decode_transaction_volume(1), 100.0);
        assert_eq!(decode_transaction_volume(10), 1000.0);
        assert_eq!(decode_transaction_volume(100), 10000.0);
    }

    #[test]
    fn test_transaction_data_clone() {
        let tx = TransactionData {
            time: 930,
            price: 10.5,
            volume: 100.0,
            direction: TransactionDirection::Buy,
        };
        let cloned = tx.clone();
        assert_eq!(cloned.time, 930);
        assert_eq!(cloned.price, 10.5);
    }

    #[test]
    fn test_transaction_time_str_edge() {
        let tx = TransactionData {
            time: 0,
            price: 0.0,
            volume: 0.0,
            direction: TransactionDirection::Neutral,
        };
        assert_eq!(tx.time_str(), "00:00");
    }
}
