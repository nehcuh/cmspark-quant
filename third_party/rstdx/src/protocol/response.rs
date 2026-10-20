//! Response parsing.

use bytes::Buf;

/// Kline data.
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

/// Security quote data.
#[derive(Debug, Clone)]
pub struct SecurityQuote {
    pub market: u8,
    pub code: String,
    pub price: f64,
    pub last_close: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub volume: f64,
    pub amount: f64,
    pub bid1: f64,
    pub ask1: f64,
    pub bid_vol1: f64,
    pub ask_vol1: f64,
}

/// Stock info.
#[derive(Debug, Clone)]
pub struct StockInfo {
    pub code: String,
    pub name: String,
}

/// Parse kline count from response body.
pub fn parse_kline_count(data: &[u8]) -> Option<u16> {
    if data.len() < 2 {
        return None;
    }
    let mut buf = &data[..2];
    Some(buf.get_u16_le())
}

/// Parse stock count from response body.
pub fn parse_stock_count(data: &[u8]) -> Option<u16> {
    if data.len() < 2 {
        return None;
    }
    let mut buf = &data[..2];
    Some(buf.get_u16_le())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kline_count() {
        let data = [0x0A, 0x00]; // 10 klines
        assert_eq!(parse_kline_count(&data), Some(10));
    }

    #[test]
    fn test_parse_kline_count_empty() {
        let data = [];
        assert_eq!(parse_kline_count(&data), None);
    }

    #[test]
    fn test_parse_stock_count() {
        let data = [0x05, 0x00]; // 5 stocks
        assert_eq!(parse_stock_count(&data), Some(5));
    }

    #[test]
    fn test_parse_stock_count_empty() {
        let data = [0x00]; // 1 byte only
        assert_eq!(parse_stock_count(&data), None);
    }

    #[test]
    fn test_parse_kline_count_large() {
        let data = [0xFF, 0x7F]; // 32767
        assert_eq!(parse_kline_count(&data), Some(32767));
    }
}
