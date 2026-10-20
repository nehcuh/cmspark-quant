//! Request serialization.

/// Market codes.
pub const MARKET_SZ: u8 = 0; // Shenzhen
pub const MARKET_SH: u8 = 1; // Shanghai
pub const MARKET_BJ: u8 = 2; // Beijing (北交所/新三板精选层)

/// Kline types.
pub const KLINE_TYPE_5MIN: u16 = 0;
pub const KLINE_TYPE_15MIN: u16 = 1;
pub const KLINE_TYPE_30MIN: u16 = 2;
pub const KLINE_TYPE_1HOUR: u16 = 3;
pub const KLINE_TYPE_DAILY: u16 = 4;
pub const KLINE_TYPE_WEEKLY: u16 = 5;
pub const KLINE_TYPE_MONTHLY: u16 = 6;
pub const KLINE_TYPE_1MIN: u16 = 8;
pub const KLINE_TYPE_RI_K: u16 = 9;

/// Build GetSecurityBars request.
/// Format: <HIHHHH6sHHHHIIH (matches pytdx exactly)
pub fn build_get_security_bars(
    category: u16,
    market: u8,
    code: &[u8; 6],
    start: u16,
    count: u16,
) -> Vec<u8> {
    let mut pkg = Vec::with_capacity(38);

    // Header: <HIHHHH
    pkg.extend_from_slice(&0x10cu16.to_le_bytes()); // magic
    pkg.extend_from_slice(&0x01016408u32.to_le_bytes()); // cmd
    pkg.extend_from_slice(&0x1cu16.to_le_bytes()); // len1
    pkg.extend_from_slice(&0x1cu16.to_le_bytes()); // len2
    pkg.extend_from_slice(&0x052du16.to_le_bytes()); // cmd2

    // Body: 6sHHHHIIH (market as 2-byte LE + code + rest)
    pkg.extend_from_slice(&(market as u16).to_le_bytes());
    pkg.extend_from_slice(code);
    pkg.extend_from_slice(&category.to_le_bytes());
    pkg.extend_from_slice(&1u16.to_le_bytes()); // reserved
    pkg.extend_from_slice(&start.to_le_bytes());
    pkg.extend_from_slice(&count.to_le_bytes());
    pkg.extend_from_slice(&0u32.to_le_bytes()); // zeros
    pkg.extend_from_slice(&0u32.to_le_bytes());
    pkg.extend_from_slice(&0u16.to_le_bytes()); // zeros

    pkg
}

/// Build GetSecurityQuotes request.
/// Format: <HIHHIIHH + stocks
pub fn build_get_security_quotes(stocks: &[(u8, &[u8; 6])]) -> Vec<u8> {
    let stock_len = stocks.len() as u16;
    let pkg_data_len = stock_len * 7 + 12;

    let mut pkg = Vec::with_capacity(pkg_data_len as usize);

    // Header: <HIHHIIHH
    pkg.extend_from_slice(&0x10cu16.to_le_bytes()); // magic (H)
    pkg.extend_from_slice(&0x02006320u32.to_le_bytes()); // cmd (I)
    pkg.extend_from_slice(&pkg_data_len.to_le_bytes()); // len1 (H)
    pkg.extend_from_slice(&pkg_data_len.to_le_bytes()); // len2 (H)
    pkg.extend_from_slice(&0x5053eu32.to_le_bytes()); // cmd2 (I)
    pkg.extend_from_slice(&0u32.to_le_bytes()); // zeros (I)
    pkg.extend_from_slice(&0u16.to_le_bytes()); // zeros (H)
    pkg.extend_from_slice(&stock_len.to_le_bytes()); // stock_len (H)

    for (market, code) in stocks {
        pkg.push(*market);
        pkg.extend_from_slice(*code);
    }

    pkg
}

/// Build GetSecurityList request.
pub fn build_get_security_list(market: u8, start: u16) -> Vec<u8> {
    let mut pkg = Vec::with_capacity(20);

    // Header: matches pytdx bytearray 0c 01 18 64 01 01 06 00 06 00 50 04
    pkg.extend_from_slice(&0x10cu16.to_le_bytes()); // magic
    pkg.extend_from_slice(&0x01016418u32.to_le_bytes()); // cmd
    pkg.extend_from_slice(&0x06u16.to_le_bytes()); // len1
    pkg.extend_from_slice(&0x06u16.to_le_bytes()); // len2
    pkg.extend_from_slice(&0x0450u16.to_le_bytes()); // cmd2

    // Body: market (2 bytes LE) + start (2 bytes LE)
    pkg.extend_from_slice(&(market as u16).to_le_bytes());
    pkg.extend_from_slice(&start.to_le_bytes());

    pkg
}

/// Build GetMinuteTimeData request.
pub fn build_get_minute_time_data(market: u8, code: &[u8; 6]) -> Vec<u8> {
    let mut pkg = Vec::with_capacity(32);

    // Header: <HIHHHH
    pkg.extend_from_slice(&0x10cu16.to_le_bytes()); // magic
    pkg.extend_from_slice(&0x01066308u32.to_le_bytes()); // cmd
    pkg.extend_from_slice(&0x13u16.to_le_bytes()); // len1 (19 bytes)
    pkg.extend_from_slice(&0x13u16.to_le_bytes()); // len2
    pkg.extend_from_slice(&0x01f4u16.to_le_bytes()); // cmd2 (500)

    // Body: Bx6sHHH
    pkg.push(market);
    pkg.push(0); // padding byte for alignment
    pkg.extend_from_slice(code);
    pkg.extend_from_slice(&0x01u16.to_le_bytes()); // unknown (1)
    pkg.extend_from_slice(&0x00u16.to_le_bytes()); // start
    pkg.extend_from_slice(&0x07u16.to_le_bytes()); // count (7 - for recent data)

    pkg
}

/// Build GetTransactionData request.
pub fn build_get_transaction_data(
    market: u8,
    code: &[u8; 6],
    start: u16,
    count: u16,
) -> Result<Vec<u8>, crate::error::Error> {
    let pkg_data_len = 13 + count as usize * 7; // Approximate
    let mut pkg = Vec::with_capacity(pkg_data_len);

    // Header: <HIHHHH
    pkg.extend_from_slice(&0x10cu16.to_le_bytes()); // magic
    pkg.extend_from_slice(&0x01076308u32.to_le_bytes()); // cmd
    pkg.extend_from_slice(&0x10u16.to_le_bytes()); // len1
    pkg.extend_from_slice(&0x10u16.to_le_bytes()); // len2
    pkg.extend_from_slice(&0x07d0u16.to_le_bytes()); // cmd2 (2000)

    // Body: Bx6sHH
    pkg.push(market);
    pkg.push(0); // padding byte for alignment
    pkg.extend_from_slice(code);
    pkg.extend_from_slice(&start.to_le_bytes());
    pkg.extend_from_slice(&count.to_le_bytes());

    Ok(pkg)
}

/// Build GetFinanceInfo request.
///
/// Gets comprehensive financial information for a stock including
/// equity structure, assets, liabilities, and income statement data.
///
/// Format matches pytdx: <HIHHHHB6s (NO padding between market and code!)
/// - magic: 0x1f0c (little endian)
/// - cmd: 0x18760001
/// - len1/len2: 11 (body size)
/// - cmd2: 0x0010
/// - extra: 0x0001
/// - market: 1 byte
/// - code: 6 bytes (NO padding!)
pub fn build_get_finance_info(market: u8, code: &[u8; 6]) -> Vec<u8> {
    let mut pkg = Vec::with_capacity(21);

    // Header - matching pytdx format exactly.
    //
    // NOTE: The command bytes `18 76 00 01` are written in big-endian order
    // to match pytdx's raw bytearray `bytearray.fromhex('0c 1f 18 76 00 01 ...')`.
    // This is a protocol-level quirk of the finance info endpoint — the value
    // 0x18760001 is *not* transmitted as little-endian like other commands.
    // Using to_be_bytes() ensures the wire format matches the reference impl.
    pkg.extend_from_slice(&0x1f0cu16.to_le_bytes()); // magic: 0x1f0c -> 0c 1f
    pkg.extend_from_slice(&0x18760001u32.to_be_bytes()); // cmd: wire bytes = 18 76 00 01
    pkg.extend_from_slice(&11u16.to_le_bytes()); // len1: 11 -> 0b 00
    pkg.extend_from_slice(&11u16.to_le_bytes()); // len2: 11 -> 0b 00
    pkg.extend_from_slice(&0x0010u16.to_le_bytes()); // cmd2: 0x0010 -> 10 00
    pkg.extend_from_slice(&0x0001u16.to_le_bytes()); // extra: 0x0001 -> 01 00

    // Body: B6s (market + code, NO padding!)
    pkg.push(market); // market: 1 byte
    pkg.extend_from_slice(code); // code: 6 bytes

    pkg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_get_security_bars() {
        let code = *b"000001";
        let pkg = build_get_security_bars(KLINE_TYPE_DAILY, MARKET_SZ, &code, 0, 10);

        assert_eq!(pkg[0], 0x0c); // magic
        assert_eq!(pkg[12], MARKET_SZ); // market as 2-byte LE low byte
        assert_eq!(pkg[13], 0); // market high byte
        assert_eq!(&pkg[14..20], b"000001"); // code (no padding)
    }

    #[test]
    fn test_build_get_security_quotes() {
        let code1 = *b"000001";
        let code2 = *b"600300";
        let pkg = build_get_security_quotes(&[(MARKET_SZ, &code1), (MARKET_SH, &code2)]);

        assert_eq!(pkg[0], 0x0c); // magic (0x10c in le)
        assert_eq!(pkg[1], 0x01);
        // stock_len is at position 20-21
        assert_eq!(pkg[20], 2); // stock count
    }

    #[test]
    fn test_build_get_security_list() {
        let pkg = build_get_security_list(MARKET_SH, 0);
        assert_eq!(pkg[0], 0x0c); // magic
        assert_eq!(pkg[12], MARKET_SH); // market
    }

    #[test]
    fn test_build_get_finance_info() {
        let code = *b"600000";
        let pkg = build_get_finance_info(MARKET_SH, &code);
        assert_eq!(pkg[0], 0x0c); // magic low byte
        assert_eq!(pkg[1], 0x1f); // magic high byte
        assert_eq!(pkg[14], MARKET_SH); // market
        assert_eq!(&pkg[15..21], b"600000"); // code
    }

    #[test]
    fn test_build_get_minute_time_data() {
        let code = *b"000001";
        let pkg = build_get_minute_time_data(MARKET_SZ, &code);
        assert_eq!(pkg[0], 0x0c); // magic
        assert_eq!(pkg[12], MARKET_SZ); // market
        assert_eq!(&pkg[14..20], b"000001"); // code
    }

    #[test]
    fn test_build_get_transaction_data() {
        let code = *b"000001";
        let pkg = build_get_transaction_data(MARKET_SZ, &code, 0, 10).unwrap();
        assert_eq!(pkg[0], 0x0c); // magic
        assert_eq!(pkg[12], MARKET_SZ); // market
    }

    #[test]
    fn test_kline_type_constants() {
        assert_eq!(KLINE_TYPE_DAILY, 4);
        assert_eq!(KLINE_TYPE_1MIN, 8);
        assert_eq!(KLINE_TYPE_RI_K, 9);
    }
}
