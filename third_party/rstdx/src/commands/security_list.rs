//! GetSecurityList command - stock list data.

use crate::client::TdxConnection;
use crate::codec::decode_volume;
use crate::error::Result;
use crate::protocol::request;
use bytes::Buf;
use serde::Serialize;
use tracing::debug;

/// Security list item.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SecurityItem {
    /// Stock code (e.g., "000001")
    pub code: String,
    /// Volume unit (lots per hand)
    pub volunit: u16,
    /// Decimal point for price (2=0.01, 3=0.001)
    pub decimal_point: u8,
    /// Stock name
    pub name: String,
    /// Previous close price
    pub pre_close: f64,
}

/// Get security list for a market.
///
/// # Arguments
/// * `market` - Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
/// * `start` - Start position for pagination
pub async fn get_security_list(
    conn: &mut TdxConnection,
    market: u8,
    start: u16,
) -> Result<Vec<SecurityItem>> {
    let pkg = request::build_get_security_list(market, start);
    conn.send(&pkg).await?;

    let body = conn.recv_response().await?;
    parse_security_list(&body)
}

/// Parse security list response body.
fn parse_security_list(body: &[u8]) -> Result<Vec<SecurityItem>> {
    debug!("body len = {}", body.len());
    debug!("body hex = {:02x?}", &body[..body.len().min(100)]);

    let mut buf = body;

    // First 2 bytes: count
    let count = buf.get_u16_le();
    debug!("count = {}, remaining bytes = {}", count, buf.len());

    // Each security record is 29 bytes: <6sH8s4sBI4s
    let expected_size = count as usize * 29;
    debug!("expected size for {} items = {}", count, expected_size);

    if buf.len() < expected_size {
        debug!(
            "WARNING: not enough bytes! have {}, need {}",
            buf.len(),
            expected_size
        );
        // Adjust count to available bytes
        let actual_count = buf.len() / 29;
        debug!("adjusting count to {}", actual_count);
    }

    let mut securities = Vec::with_capacity(count as usize);
    let mut i = 0;
    while buf.len() >= 29 && i < count as usize {
        let item = parse_one_security(&mut buf)?;
        securities.push(item);
        i += 1;
    }

    debug!(
        "parsed {} items, remaining bytes = {}",
        securities.len(),
        buf.len()
    );

    Ok(securities)
}

/// Parse a single security item (29 bytes).
fn parse_one_security(buf: &mut &[u8]) -> Result<SecurityItem> {
    // Code: 6 bytes
    let mut code_bytes = [0u8; 6];
    buf.copy_to_slice(&mut code_bytes);

    // Volunit: 2 bytes
    let volunit = buf.get_u16_le();

    // Name: 8 bytes (GBK encoded)
    let mut name_bytes = [0u8; 8];
    buf.copy_to_slice(&mut name_bytes);

    // Reversed bytes 1: 4 bytes
    let mut _reversed1 = [0u8; 4];
    buf.copy_to_slice(&mut _reversed1);

    // Decimal point: 1 byte
    let decimal_point = buf.get_u8();

    // Pre close raw: 4 bytes
    let pre_close_raw = buf.get_u32_le();

    // Reversed bytes 2: 4 bytes
    let mut _reversed2 = [0u8; 4];
    buf.copy_to_slice(&mut _reversed2);

    // Decode code to string
    let code = String::from_utf8_lossy(&code_bytes)
        .trim_end_matches('\0')
        .to_string();

    // Decode name from GBK
    let name = decode_gbk(&name_bytes);

    // Decode pre close price
    let pre_close = decode_volume(pre_close_raw).unwrap_or(0.0);

    Ok(SecurityItem {
        code,
        volunit,
        decimal_point,
        name,
        pre_close,
    })
}

/// Decode GBK encoded bytes to String.
fn decode_gbk(bytes: &[u8]) -> String {
    // Find null terminator
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let gbk_bytes = &bytes[..end];

    // Try to decode as GBK, fallback to lossy UTF-8
    encoding_rs::GBK.decode(gbk_bytes).0.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_gbk() {
        // Test basic ASCII
        assert_eq!(decode_gbk(b"ABC\0\xff\xff\xff\xff\xff\xff\xff"), "ABC");

        // Test empty/null terminated
        assert_eq!(decode_gbk(b"\0\0\0\0\0\0\0\0"), "");
    }

    #[test]
    fn test_security_item_size() {
        // Verify each security record is 29 bytes
        assert_eq!(6 + 2 + 8 + 4 + 1 + 4 + 4, 29);
    }

    #[test]
    fn test_parse_security_list_empty() {
        let body = [0x00, 0x00]; // count = 0
        let result = parse_security_list(&body).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_security_list_single() {
        let mut body = vec![0x01, 0x00]; // count = 1

        // Code: 6 bytes
        body.extend_from_slice(b"000001");
        // Volunit: 2 bytes
        body.extend_from_slice(&100u16.to_le_bytes());
        // Name: 8 bytes (ASCII "ABC" padded with nulls)
        body.extend_from_slice(b"ABC\0\0\0\0\0");
        // Reversed1: 4 bytes
        body.extend_from_slice(&[0u8; 4]);
        // Decimal point: 1 byte
        body.push(2);
        // Pre close raw: 4 bytes
        body.extend_from_slice(&1000u32.to_le_bytes());
        // Reversed2: 4 bytes
        body.extend_from_slice(&[0u8; 4]);

        let result = parse_security_list(&body).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code, "000001");
        assert_eq!(result[0].volunit, 100);
        assert_eq!(result[0].decimal_point, 2);
        assert_eq!(result[0].name, "ABC");
    }

    #[test]
    fn test_parse_security_list_multiple() {
        let mut body = vec![0x02, 0x00]; // count = 2

        for code in [b"000001", b"600000"] {
            body.extend_from_slice(code);
            body.extend_from_slice(&100u16.to_le_bytes());
            body.extend_from_slice(b"\0\0\0\0\0\0\0\0");
            body.extend_from_slice(&[0u8; 4]);
            body.push(2);
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&[0u8; 4]);
        }

        let result = parse_security_list(&body).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].code, "000001");
        assert_eq!(result[1].code, "600000");
    }

    #[test]
    fn test_decode_gbk_chinese() {
        // "中" in GBK is 0xD6 0xD0
        let gbk_bytes = [0xD6, 0xD0, 0, 0, 0, 0, 0, 0];
        let name = decode_gbk(&gbk_bytes);
        assert!(!name.is_empty());
    }
}
