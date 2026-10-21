//! GetSecurityQuotes command - stock quote snapshot data.

use crate::client::TdxConnection;
use crate::codec::{decode_price, decode_volume};
use crate::error::Result;
use crate::protocol::request;
use bytes::Buf;
use serde::Serialize;

/// Security quote (snapshot) data.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SecurityQuote {
    /// Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
    pub market: u8,
    /// Stock code
    pub code: String,
    /// Current price
    pub price: f64,
    /// Last close price
    pub last_close: f64,
    /// Open price
    pub open: f64,
    /// High price
    pub high: f64,
    /// Low price
    pub low: f64,
    /// Volume (in lots/hands)
    pub volume: f64,
    /// Amount (trading value)
    pub amount: f64,
    /// Bid 1 price
    pub bid1: f64,
    /// Ask 1 price
    pub ask1: f64,
    /// Bid 1 volume
    pub bid_vol1: f64,
    /// Ask 1 volume
    pub ask_vol1: f64,
    /// Bid 2 price
    pub bid2: f64,
    /// Ask 2 price
    pub ask2: f64,
    /// Bid 2 volume
    pub bid_vol2: f64,
    /// Ask 2 volume
    pub ask_vol2: f64,
    /// Bid 3 price
    pub bid3: f64,
    /// Ask 3 price
    pub ask3: f64,
    /// Bid 3 volume
    pub bid_vol3: f64,
    /// Ask 3 volume
    pub ask_vol3: f64,
    /// Bid 4 price
    pub bid4: f64,
    /// Ask 4 price
    pub ask4: f64,
    /// Bid 4 volume
    pub bid_vol4: f64,
    /// Ask 4 volume
    pub ask_vol4: f64,
    /// Bid 5 price
    pub bid5: f64,
    /// Ask 5 price
    pub ask5: f64,
    /// Bid 5 volume
    pub bid_vol5: f64,
    /// Ask 5 volume
    pub ask_vol5: f64,
}

impl SecurityQuote {
    /// Get price change percentage.
    pub fn change_pct(&self) -> f64 {
        if self.last_close == 0.0 {
            0.0
        } else {
            (self.price - self.last_close) / self.last_close * 100.0
        }
    }
}

/// Parse price value from buffer.
fn decode_price_value(buf: &mut &[u8]) -> Result<i32> {
    let (value, pos) = decode_price(buf, 0)?;
    *buf = &buf[pos..];
    Ok(value)
}

/// Calculate actual price from base and diff.
fn calculate_price(base: i32, diff: i32) -> f64 {
    (base + diff) as f64 / 100.0
}

/// Get security quotes for multiple stocks.
pub async fn get_security_quotes(
    conn: &mut TdxConnection,
    stocks: &[(u8, &[u8; 6])],
) -> Result<Vec<SecurityQuote>> {
    let pkg = request::build_get_security_quotes(stocks);
    conn.send(&pkg).await?;

    let body = conn.recv_response().await?;
    parse_security_quotes(&body)
}

/// Parse security quotes response body.
fn parse_security_quotes(body: &[u8]) -> Result<Vec<SecurityQuote>> {
    let mut buf = body;

    // Skip first 2 bytes + count (2 bytes)
    ensure_bytes!(buf, 4);
    buf.advance(2);

    let count = buf.get_u16_le();
    let mut quotes = Vec::with_capacity(count as usize);

    for _ in 0..count {
        let quote = parse_one_quote(&mut buf)?;
        quotes.push(quote);
    }

    Ok(quotes)
}

/// Parse a single security quote.
fn parse_one_quote(buf: &mut &[u8]) -> Result<SecurityQuote> {
    // Minimum: market(1) + code(6) + active1(2) + price fields + trailing fields
    ensure_bytes!(*buf, 56);

    // Market (1 byte) + Code (6 bytes) + Active1 (2 bytes)
    let market = buf.get_u8();
    let mut code_bytes = [0u8; 6];
    buf.copy_to_slice(&mut code_bytes);
    let _active1 = buf.get_u16_le();

    // Price base and diffs
    let price_base = decode_price_value(buf)?;
    let last_close_diff = decode_price_value(buf)?;
    let open_diff = decode_price_value(buf)?;
    let high_diff = decode_price_value(buf)?;
    let low_diff = decode_price_value(buf)?;

    // reversed_bytes0 and reversed_bytes1 (server time / reserved)
    let _reversed0 = decode_price_value(buf)?;
    let _reversed1 = decode_price_value(buf)?;

    // Volume and amount
    let vol_raw = decode_price_value(buf)?;
    let _cur_vol = decode_price_value(buf)?;
    let amount_raw = buf.get_u32_le();

    // Skip s_vol, b_vol, reversed_bytes2, reversed_bytes3
    let _s_vol = decode_price_value(buf)?;
    let _b_vol = decode_price_value(buf)?;
    let _reversed2 = decode_price_value(buf)?;
    let _reversed3 = decode_price_value(buf)?;

    // Parse bid/ask levels
    let bid1_diff = decode_price_value(buf)?;
    let ask1_diff = decode_price_value(buf)?;
    let bid_vol1_raw = decode_price_value(buf)?;
    let ask_vol1_raw = decode_price_value(buf)?;

    let bid2_diff = decode_price_value(buf)?;
    let ask2_diff = decode_price_value(buf)?;
    let bid_vol2_raw = decode_price_value(buf)?;
    let ask_vol2_raw = decode_price_value(buf)?;

    let bid3_diff = decode_price_value(buf)?;
    let ask3_diff = decode_price_value(buf)?;
    let bid_vol3_raw = decode_price_value(buf)?;
    let ask_vol3_raw = decode_price_value(buf)?;

    let bid4_diff = decode_price_value(buf)?;
    let ask4_diff = decode_price_value(buf)?;
    let bid_vol4_raw = decode_price_value(buf)?;
    let ask_vol4_raw = decode_price_value(buf)?;

    let bid5_diff = decode_price_value(buf)?;
    let ask5_diff = decode_price_value(buf)?;
    let bid_vol5_raw = decode_price_value(buf)?;
    let ask_vol5_raw = decode_price_value(buf)?;

    // Skip trailing fields that follow the 5 bid/ask levels.
    // Matches pytdx: reversed_bytes4 (2 bytes) + 4 get_price values
    // + reversed_bytes9/active2 (4 bytes).
    ensure_bytes!(*buf, 2 + 4 + 4);
    buf.advance(2);
    let _reversed_bytes5 = decode_price_value(buf)?;
    let _reversed_bytes6 = decode_price_value(buf)?;
    let _reversed_bytes7 = decode_price_value(buf)?;
    let _reversed_bytes8 = decode_price_value(buf)?;
    buf.advance(4);

    // Calculate prices from base
    let price = calculate_price(price_base, 0);
    let last_close = calculate_price(price_base, last_close_diff);
    let open = calculate_price(price_base, open_diff);
    let high = calculate_price(price_base, high_diff);
    let low = calculate_price(price_base, low_diff);

    let bid1 = calculate_price(price_base, bid1_diff);
    let ask1 = calculate_price(price_base, ask1_diff);
    let bid2 = calculate_price(price_base, bid2_diff);
    let ask2 = calculate_price(price_base, ask2_diff);
    let bid3 = calculate_price(price_base, bid3_diff);
    let ask3 = calculate_price(price_base, ask3_diff);
    let bid4 = calculate_price(price_base, bid4_diff);
    let ask4 = calculate_price(price_base, ask4_diff);
    let bid5 = calculate_price(price_base, bid5_diff);
    let ask5 = calculate_price(price_base, ask5_diff);

    // Helper to convert raw volume to f64 (0 when market is closed)
    let to_vol = |raw: i32| if raw > 0 { raw as f64 } else { 0.0 };

    // Decode volumes
    let volume = to_vol(vol_raw);
    let amount = decode_volume(amount_raw).unwrap_or(0.0);

    // Bid/Ask volumes - these are typically 0 when market is closed
    let bid_vol1 = to_vol(bid_vol1_raw);
    let ask_vol1 = to_vol(ask_vol1_raw);
    let bid_vol2 = to_vol(bid_vol2_raw);
    let ask_vol2 = to_vol(ask_vol2_raw);
    let bid_vol3 = to_vol(bid_vol3_raw);
    let ask_vol3 = to_vol(ask_vol3_raw);
    let bid_vol4 = to_vol(bid_vol4_raw);
    let ask_vol4 = to_vol(ask_vol4_raw);
    let bid_vol5 = to_vol(bid_vol5_raw);
    let ask_vol5 = to_vol(ask_vol5_raw);

    // Convert code to string
    let code = String::from_utf8_lossy(&code_bytes)
        .trim_end_matches('\0')
        .to_string();

    Ok(SecurityQuote {
        market,
        code,
        price,
        last_close,
        open,
        high,
        low,
        volume,
        amount,
        bid1,
        ask1,
        bid_vol1,
        ask_vol1,
        bid2,
        ask2,
        bid_vol2,
        ask_vol2,
        bid3,
        ask3,
        bid_vol3,
        ask_vol3,
        bid4,
        ask4,
        bid_vol4,
        ask_vol4,
        bid5,
        ask5,
        bid_vol5,
        ask_vol5,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_price() {
        // Quote prices use 2 decimal places (divide by 100)
        assert_eq!(calculate_price(10000, 50), 100.5);
        assert_eq!(calculate_price(10000, -50), 99.5);
        assert_eq!(calculate_price(0, 0), 0.0);
    }

    #[test]
    fn test_parse_security_quotes_empty() {
        // skip(2) + count(2) = 4 bytes, count = 0
        let body = [0x00, 0x00, 0x00, 0x00];
        let result = parse_security_quotes(&body).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_one_quote_basic() {
        use crate::codec::encode_price;

        let mut body = vec![
            0x00, 0x00, // skip 2 bytes
            0x01, 0x00, // count = 1
        ];

        // Market (1) + Code (6) + Active1 (2)
        body.push(1); // market = Shanghai
        body.extend_from_slice(b"600000"); // code
        body.extend_from_slice(&0u16.to_le_bytes()); // active1

        // Price base and diffs (all 0 for simplicity)
        for _ in 0..4 {
            body.extend_from_slice(&encode_price(0));
        }

        // Skip 2 reversed price values
        for _ in 0..2 {
            body.extend_from_slice(&encode_price(0));
        }

        // vol_raw, cur_vol
        body.extend_from_slice(&encode_price(0));
        body.extend_from_slice(&encode_price(0));

        // amount_raw (4 bytes)
        body.extend_from_slice(&0u32.to_le_bytes());

        // Skip 4 values + 1 extra padding to meet minimum size
        for _ in 0..5 {
            body.extend_from_slice(&encode_price(0));
        }

        // Bid/Ask levels (5 levels × 4 values each = 20 values)
        for _ in 0..20 {
            body.extend_from_slice(&encode_price(0));
        }

        // Trailing: reversed_bytes4 (2 bytes) + 4 get_price values + reversed_bytes9/active2 (4 bytes)
        body.extend_from_slice(&0u16.to_le_bytes());
        for _ in 0..4 {
            body.extend_from_slice(&encode_price(0));
        }
        body.extend_from_slice(&0u32.to_le_bytes());

        let result = parse_security_quotes(&body).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].market, 1);
        assert_eq!(result[0].code, "600000");
    }

    #[test]
    fn test_parse_security_quotes_buffer_too_short() {
        let body = [0x00];
        let result = parse_security_quotes(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_security_quote_change_pct() {
        let quote = SecurityQuote {
            market: 1,
            code: "600000".to_string(),
            price: 11.0,
            last_close: 10.0,
            open: 10.0,
            high: 11.5,
            low: 9.8,
            volume: 1000000.0,
            amount: 11000000.0,
            bid1: 10.99,
            ask1: 11.01,
            bid_vol1: 100.0,
            ask_vol1: 200.0,
            bid2: 0.0,
            ask2: 0.0,
            bid_vol2: 0.0,
            ask_vol2: 0.0,
            bid3: 0.0,
            ask3: 0.0,
            bid_vol3: 0.0,
            ask_vol3: 0.0,
            bid4: 0.0,
            ask4: 0.0,
            bid_vol4: 0.0,
            ask_vol4: 0.0,
            bid5: 0.0,
            ask5: 0.0,
            bid_vol5: 0.0,
            ask_vol5: 0.0,
        };
        assert_eq!(quote.change_pct(), 10.0);
    }

    #[test]
    fn test_security_quote_change_pct_zero_last_close() {
        let quote = SecurityQuote {
            market: 1,
            code: "600000".to_string(),
            price: 11.0,
            last_close: 0.0,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            volume: 0.0,
            amount: 0.0,
            bid1: 0.0,
            ask1: 0.0,
            bid_vol1: 0.0,
            ask_vol1: 0.0,
            bid2: 0.0,
            ask2: 0.0,
            bid_vol2: 0.0,
            ask_vol2: 0.0,
            bid3: 0.0,
            ask3: 0.0,
            bid_vol3: 0.0,
            ask_vol3: 0.0,
            bid4: 0.0,
            ask4: 0.0,
            bid_vol4: 0.0,
            ask_vol4: 0.0,
            bid5: 0.0,
            ask5: 0.0,
            bid_vol5: 0.0,
            ask_vol5: 0.0,
        };
        assert_eq!(quote.change_pct(), 0.0);
    }
}
