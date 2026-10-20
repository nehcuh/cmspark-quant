//! GetSecurityBars command - K-line data retrieval.

use crate::client::TdxConnection;
use crate::codec::{DateTimeFormat, decode_datetime, decode_price, decode_volume};
use crate::error::Result;
use crate::protocol::request;
use bytes::Buf;

/// K-line data.
#[derive(Debug, Clone, PartialEq)]
pub struct Kline {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub amount: f64,
}

impl Kline {
    /// Format as datetime string.
    pub fn datetime(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }
}

/// Get K-line data.
pub async fn get_security_bars(
    conn: &mut TdxConnection,
    category: u16,
    market: u8,
    code: &[u8; 6],
    start: u16,
    count: u16,
) -> Result<Vec<Kline>> {
    let pkg = request::build_get_security_bars(category, market, code, start, count);
    conn.send(&pkg).await?;

    let body = conn.recv_response().await?;
    parse_security_bars(&body, category)
}

fn parse_security_bars(body: &[u8], category: u16) -> Result<Vec<Kline>> {
    let mut buf = body;
    // Some servers (e.g. when they refuse to serve the client) answer with an empty
    // body or a bare count header and no records. Treat that as "no bars" instead of
    // failing with a misleading datetime decode error.
    if buf.remaining() == 0 {
        return Ok(Vec::new());
    }
    ensure_bytes!(buf, 2);
    let count = buf.get_u16_le();
    if buf.remaining() == 0 {
        return Ok(Vec::new());
    }
    let mut klines = Vec::with_capacity(count as usize);

    let mut pre_diff_base = 0i32;

    for _ in 0..count {
        let dt_format = if category < 4 || category == 7 || category == 8 {
            DateTimeFormat::Minute
        } else {
            DateTimeFormat::Daily
        };

        let (year, month, day, hour, minute, pos) = decode_datetime(buf, 0, dt_format)?;
        buf = &buf[pos..];

        let price_open_diff = decode_price_value(&mut buf)?;
        let price_close_diff = decode_price_value(&mut buf)?;
        let price_high_diff = decode_price_value(&mut buf)?;
        let price_low_diff = decode_price_value(&mut buf)?;

        ensure_bytes!(buf, 8);
        let vol_raw = buf.get_u32_le();
        let volume = decode_volume(vol_raw)?;

        let amount_raw = buf.get_u32_le();
        let amount = decode_volume(amount_raw)?;

        let open = calculate_price(price_open_diff, pre_diff_base);
        let price_open_diff = price_open_diff + pre_diff_base;

        let close = calculate_price(price_open_diff, price_close_diff);
        let high = calculate_price(price_open_diff, price_high_diff);
        let low = calculate_price(price_open_diff, price_low_diff);

        pre_diff_base = price_open_diff + price_close_diff;

        klines.push(Kline {
            year,
            month,
            day,
            hour,
            minute,
            open,
            high,
            low,
            close,
            volume,
            amount,
        });
    }

    Ok(klines)
}

fn decode_price_value(buf: &mut &[u8]) -> Result<i32> {
    let (value, pos) = decode_price(buf, 0)?;
    *buf = &buf[pos..];
    Ok(value)
}

fn calculate_price(base: i32, diff: i32) -> f64 {
    (base + diff) as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_or_header_only_body_is_zero_bars() {
        assert!(super::parse_security_bars(&[], 4).unwrap().is_empty());
        // observed in the wild: 2-byte body (count field only, no records)
        assert!(super::parse_security_bars(&[0x20, 0x03], 4).unwrap().is_empty());
        assert!(super::parse_security_bars(&[0x20, 0x03], 9).unwrap().is_empty());
    }

    #[test]
    fn truncated_record_is_error_not_panic() {
        // count=1, a full daily datetime, then nothing
        let body = [0x01, 0x00, 0x14, 0x34, 0x35, 0x01];
        assert!(super::parse_security_bars(&body, 4).is_err());
    }

    use super::*;

    #[test]
    fn test_kline_datetime_format() {
        let kline = Kline {
            year: 2024,
            month: 4,
            day: 22,
            hour: 10,
            minute: 30,
            open: 10.0,
            high: 11.0,
            low: 9.5,
            close: 10.5,
            volume: 1000000.0,
            amount: 10000000.0,
        };

        assert_eq!(kline.datetime(), "2024-04-22 10:30");
    }

    #[test]
    fn test_calculate_price() {
        assert_eq!(calculate_price(10000, 50), 10.05);
        assert_eq!(calculate_price(10000, -50), 9.95);
    }

    #[test]
    fn test_parse_security_bars_empty() {
        // Empty response: count = 0
        let body = [0x00, 0x00];
        let result = parse_security_bars(&body, 4).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_security_bars_single_daily() {
        use crate::codec::{encode_datetime, encode_price};

        let mut body = vec![0x01, 0x00]; // count = 1

        // Daily datetime: 2024-04-22
        body.extend_from_slice(&encode_datetime(2024, 4, 22, 15, 0, DateTimeFormat::Daily));

        // Price diffs (from base 0): open=10000, close=50, high=100, low=-50
        body.extend_from_slice(&encode_price(10000));
        body.extend_from_slice(&encode_price(50));
        body.extend_from_slice(&encode_price(100));
        body.extend_from_slice(&encode_price(-50));

        // Volume and amount raw
        body.extend_from_slice(&100u32.to_le_bytes());
        body.extend_from_slice(&1000u32.to_le_bytes());

        let result = parse_security_bars(&body, 4).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].datetime(), "2024-04-22 15:00");
        assert_eq!(result[0].open, 10.0);
        assert_eq!(result[0].close, 10.05);
        assert_eq!(result[0].high, 10.10);
        assert_eq!(result[0].low, 9.95);
    }

    #[test]
    fn test_parse_security_bars_two_records() {
        use crate::codec::{encode_datetime, encode_price};

        let mut body = vec![0x02, 0x00]; // count = 2

        // Record 1: 2024-04-22, open=10000
        body.extend_from_slice(&encode_datetime(2024, 4, 22, 15, 0, DateTimeFormat::Daily));
        body.extend_from_slice(&encode_price(10000));
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&100u32.to_le_bytes());
        body.extend_from_slice(&1000u32.to_le_bytes());

        // Record 2: 2024-04-23, open=10100 (diff from prev base 10000)
        body.extend_from_slice(&encode_datetime(2024, 4, 23, 15, 0, DateTimeFormat::Daily));
        body.extend_from_slice(&encode_price(100)); // diff from prev base
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&100u32.to_le_bytes());
        body.extend_from_slice(&1000u32.to_le_bytes());

        let result = parse_security_bars(&body, 4).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].open, 10.0);
        // Second record's open is calculated from cumulative base
        assert_eq!(result[1].open, 10.10);
    }

    #[test]
    fn test_parse_security_bars_minute_format() {
        use crate::codec::{encode_datetime, encode_price};

        let mut body = vec![0x01, 0x00]; // count = 1

        // Minute datetime: 2024-04-22 10:30 (category < 4 uses Minute format)
        body.extend_from_slice(&encode_datetime(
            2024,
            4,
            22,
            10,
            30,
            DateTimeFormat::Minute,
        ));

        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&100u32.to_le_bytes());
        body.extend_from_slice(&1000u32.to_le_bytes());

        let result = parse_security_bars(&body, 0).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].datetime(), "2024-04-22 10:30");
    }

    #[test]
    fn test_parse_security_bars_buffer_too_short() {
        // Only 1 byte, but need at least 2 for count
        let body = [0x01];
        let result = parse_security_bars(&body, 4);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_security_bars_invalid_record() {
        // count = 1 but body too short for a full record
        let body = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00];
        let result = parse_security_bars(&body, 4);
        assert!(result.is_err());
    }

    #[test]
    fn test_kline_clone() {
        let kline = Kline {
            year: 2024,
            month: 4,
            day: 22,
            hour: 10,
            minute: 30,
            open: 10.0,
            high: 11.0,
            low: 9.5,
            close: 10.5,
            volume: 1000000.0,
            amount: 10000000.0,
        };
        let cloned = kline.clone();
        assert_eq!(cloned.year, 2024);
        assert_eq!(cloned.open, 10.0);
    }

    #[test]
    fn test_kline_equality() {
        let k1 = Kline {
            year: 2024,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 100.0,
            amount: 1000.0,
        };
        let k2 = Kline {
            year: 2024,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 100.0,
            amount: 1000.0,
        };
        assert_eq!(k1, k2);
    }
}
