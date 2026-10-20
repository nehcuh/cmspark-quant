//! GetMinuteTimeData command - intraday time-sharing data.

use crate::client::TdxConnection;
use crate::codec::{decode_price, decode_volume};
use crate::error::Result;
use crate::protocol::request;
use bytes::Buf;
use chrono::Datelike;

/// Minute time data point.
#[derive(Debug, Clone, PartialEq)]
pub struct MinuteTimeData {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub price: f64,
    pub volume: f64,
    pub amount: f64,
}

impl MinuteTimeData {
    /// Format as datetime string.
    pub fn datetime(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }
}

/// Get minute time data for a security.
///
/// # Arguments
/// * `conn` - TDX connection
/// * `market` - Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
/// * `code` - Stock code (6 bytes)
///
/// # Returns
/// List of minute data points, from most recent to oldest
pub async fn get_minute_time_data(
    conn: &mut TdxConnection,
    market: u8,
    code: &[u8; 6],
) -> Result<Vec<MinuteTimeData>> {
    let pkg = request::build_get_minute_time_data(market, code);
    conn.send(&pkg).await?;

    let body = conn.recv_response().await?;
    parse_minute_time_data(&body)
}

/// Parse minute time data response body.
fn parse_minute_time_data(body: &[u8]) -> Result<Vec<MinuteTimeData>> {
    let mut buf = body;

    // Skip first byte (unknown) + count (1 byte)
    ensure_bytes!(buf, 2);
    buf.advance(1);

    // Count of data points (1 byte)
    let count = buf.get_u8();
    let mut data_points = Vec::with_capacity(count as usize);

    // Parse each data point
    for i in 0..count {
        let data = parse_minute_data_point(&mut buf, i as usize)?;
        data_points.push(data);
    }

    Ok(data_points)
}

/// Parse a single minute data point (32 bytes per point).
///
/// NOTE: The TDX minute-time response does not include explicit time fields.
/// Time is inferred from the data-point index (sequential minutes from market open).
fn parse_minute_data_point(buf: &mut &[u8], index: usize) -> Result<MinuteTimeData> {
    // Minimum: unknown(1) + flag(2) + code(6) + price(min 1) + vol(4) + amount(4) = 18 bytes
    ensure_bytes!(*buf, 18);

    // First part: raw bytes structure
    // 1 byte: unknown (usually 0x01)
    let _unknown = buf.get_u8();

    // 2 bytes: some flag/count
    let _flag = buf.get_u16_le();

    // 6 bytes: stock code (repeated)
    let mut _code_bytes = [0u8; 6];
    buf.copy_to_slice(&mut _code_bytes);

    // Price data (variable-length encoded)
    let (price_raw, bytes_read) = decode_price(buf, 0)?;
    *buf = &buf[bytes_read..];

    // Volume and amount
    let vol_raw = buf.get_u32_le();
    let amount_raw = buf.get_u32_le();

    // Skip 2 bytes
    buf.advance(2);

    // Get current date from system (response does not include date)
    let now = chrono::Utc::now();
    let year = now.year() as u16;
    let month = now.month() as u8;
    let day = now.day() as u8;

    // Price calculation (divide by 100 for 2 decimal places)
    let price = price_raw as f64 / 100.0;

    // Volume decode
    let volume = decode_volume(vol_raw).unwrap_or(vol_raw as f64);

    // Amount decode
    let amount = decode_volume(amount_raw).unwrap_or(amount_raw as f64);

    // Infer time from sequential index (market opens at 9:30).
    // A-shares trading hours: 09:30-11:30 (120 min), 13:00-15:00 (120 min).
    let minutes_from_open = index as u32;
    const MORNING_MINUTES: u32 = 120; // 9:30-11:30
    const LUNCH_BREAK_MINUTES: u32 = 90; // 11:30-13:00

    let minutes_since_0930 = if minutes_from_open < MORNING_MINUTES {
        minutes_from_open // morning session
    } else {
        minutes_from_open + LUNCH_BREAK_MINUTES // afternoon session, skip lunch
    };

    let total_minutes = 570 + minutes_since_0930; // 570 = 9*60 + 30
    let hour = (total_minutes / 60) as u8;
    let minute = (total_minutes % 60) as u8;

    Ok(MinuteTimeData {
        year,
        month,
        day,
        hour,
        minute,
        price,
        volume,
        amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minute_time_data_datetime_format() {
        let data = MinuteTimeData {
            year: 2024,
            month: 4,
            day: 24,
            hour: 10,
            minute: 30,
            price: 10.5,
            volume: 1000000.0,
            amount: 10000000.0,
        };

        assert_eq!(data.datetime(), "2024-04-24 10:30");
    }

    #[test]
    fn test_parse_minute_time_data_empty() {
        let body = [0x00, 0x00]; // count = 0
        let result = parse_minute_time_data(&body).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_minute_time_data_single() {
        let mut body = vec![0x00, 0x01]; // unknown=0, count = 1

        // Data point: unknown(1) + flag(2) + code(6) + price(1) + vol(4) + amount(4) + skip(2) = 20 bytes
        body.push(0x01); // unknown
        body.extend_from_slice(&0u16.to_le_bytes()); // flag
        body.extend_from_slice(b"000001"); // code
        body.push(0); // price = 0
        body.extend_from_slice(&100u32.to_le_bytes()); // vol
        body.extend_from_slice(&1000u32.to_le_bytes()); // amount
        body.extend_from_slice(&0u16.to_le_bytes()); // skip

        let result = parse_minute_time_data(&body).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].price, 0.0);
    }

    #[test]
    fn test_parse_minute_time_data_buffer_too_short() {
        let body = [0x01]; // count byte incomplete
        let result = parse_minute_time_data(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_infer_trading_time_boundaries() {
        // Helper to build a minimal data point body
        fn make_body(count: u8) -> Vec<u8> {
            let mut body = vec![0x00, count]; // unknown=0, count
            for _ in 0..count {
                body.push(0x01); // unknown
                body.extend_from_slice(&0u16.to_le_bytes()); // flag
                body.extend_from_slice(b"000001"); // code
                body.push(0); // price = 0 (1 byte)
                body.extend_from_slice(&100u32.to_le_bytes()); // vol
                body.extend_from_slice(&1000u32.to_le_bytes()); // amount
                body.extend_from_slice(&0u16.to_le_bytes()); // skip
            }
            body
        }

        // Morning open (index 0)
        let body = make_body(1);
        let result = parse_minute_time_data(&body).unwrap();
        assert_eq!(result[0].hour, 9);
        assert_eq!(result[0].minute, 30);

        // Full day: 240 data points
        let body = make_body(240);
        let result = parse_minute_time_data(&body).unwrap();

        // Just before lunch (index 119 -> 11:29)
        assert_eq!(result[119].hour, 11);
        assert_eq!(result[119].minute, 29);

        // Lunch end / afternoon open (index 120 -> 13:00)
        assert_eq!(result[120].hour, 13);
        assert_eq!(result[120].minute, 0);

        // Afternoon (index 121 -> 13:01)
        assert_eq!(result[121].hour, 13);
        assert_eq!(result[121].minute, 1);

        // Market close (index 239 -> 14:59)
        assert_eq!(result[239].hour, 14);
        assert_eq!(result[239].minute, 59);
    }
}
