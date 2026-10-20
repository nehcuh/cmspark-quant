//! GetFinanceInfo command - stock financial information.

use crate::client::TdxConnection;
use crate::error::Result;
use crate::protocol::request;
use bytes::Buf;
use serde::Serialize;

/// Stock financial information.
///
/// This contains comprehensive financial data about a stock including
/// equity structure, assets, liabilities, and income statement data.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FinanceInfo {
    /// Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
    pub market: u8,
    /// Stock code
    pub code: String,
    /// Province code
    pub province: u16,
    /// Industry code
    pub industry: u16,
    /// Last update date (YYYYMMDD)
    pub updated_date: u32,
    /// IPO date (YYYYMMDD)
    pub ipo_date: u32,

    // Equity structure (in shares)
    /// Floating shares (流通股本)
    pub floating_shares: f64,
    /// Total shares (总股本)
    pub total_shares: f64,
    /// State shares (国家股)
    pub state_shares: f64,
    /// Promoter legal person shares (发起人法人股)
    pub promoter_legal_shares: f64,
    /// Legal person shares (法人股)
    pub legal_shares: f64,
    /// B-shares
    pub b_shares: f64,
    /// H-shares
    pub h_shares: f64,
    /// Employee shares (职工股)
    pub employee_shares: f64,

    // Assets (in yuan)
    /// Total assets (总资产)
    pub total_assets: f64,
    /// Current assets (流动资产)
    pub current_assets: f64,
    /// Fixed assets (固定资产)
    pub fixed_assets: f64,
    /// Intangible assets (无形资产)
    pub intangible_assets: f64,

    // Liabilities (in yuan)
    /// Current liabilities (流动负债)
    pub current_liabilities: f64,
    /// Long-term liabilities (长期负债)
    pub long_term_liabilities: f64,
    /// Capital reserve (资本公积金)
    pub capital_reserve: f64,
    /// Net assets (净资产)
    pub net_assets: f64,

    // Income statement (in yuan)
    /// Main revenue (主营收入)
    pub main_revenue: f64,
    /// Main profit (主营利润)
    pub main_profit: f64,
    /// Accounts receivable (应收账款)
    pub accounts_receivable: f64,
    /// Operating profit (营业利润)
    pub operating_profit: f64,
    /// Investment income (投资收益)
    pub investment_income: f64,

    // Cash flow (in yuan)
    /// Operating cash flow (经营现金流)
    pub operating_cash_flow: f64,
    /// Total cash flow (总现金流)
    pub total_cash_flow: f64,

    // Other metrics
    /// Inventory (存货)
    pub inventory: f64,
    /// Total profit (利润总额)
    pub total_profit: f64,
    /// After-tax profit (税后利润)
    pub after_tax_profit: f64,
    /// Net profit (净利润)
    pub net_profit: f64,
    /// Retained earnings (未分配利润)
    pub retained_earnings: f64,
    /// Net assets per share (每股净资产)
    pub net_assets_per_share: f64,

    // Additional fields
    /// Number of shareholders (股东人数)
    pub shareholder_count: f64,
    /// Reserved field 2
    pub reserved2: f64,
}

impl FinanceInfo {
    /// Format IPO date as string.
    pub fn ipo_date_str(&self) -> String {
        format_date(self.ipo_date)
    }

    /// Format update date as string.
    pub fn updated_date_str(&self) -> String {
        format_date(self.updated_date)
    }

    /// Get market name.
    pub fn market_name(&self) -> &'static str {
        match self.market {
            0 => "深圳",
            1 => "上海",
            _ => "未知",
        }
    }
}

/// Format date from YYYYMMDD to YYYY-MM-DD.
fn format_date(date: u32) -> String {
    let year = date / 10000;
    let month = (date % 10000) / 100;
    let day = date % 100;
    format!("{:04}-{:02}-{:02}", year, month, day)
}

/// Get financial information for a security.
///
/// # Arguments
/// * `conn` - TDX connection
/// * `market` - Market code (0=Shenzhen, 1=Shanghai, 2=Beijing)
/// * `code` - Stock code (6 bytes)
///
/// # Returns
/// Financial information for the stock
pub async fn get_finance_info(
    conn: &mut TdxConnection,
    market: u8,
    code: &[u8; 6],
) -> Result<FinanceInfo> {
    let pkg = request::build_get_finance_info(market, code);
    conn.send(&pkg).await?;
    let body = conn.recv_response().await?;
    parse_finance_info(&body)
}

/// Parse finance info response body.
///
/// NOTE: The TDX protocol transmits financial values as IEEE-754 `f32`
/// (single-precision float). After scaling by 10_000, very large values
/// (e.g. total assets in the trillions of yuan) may lose ~1-2 decimal
/// digits of precision compared to the server's internal representation.
/// This matches pytdx behaviour and is a protocol limitation.
fn parse_finance_info(body: &[u8]) -> Result<FinanceInfo> {
    let mut buf = body;

    // Minimum body size: 2 (count) + 1 (market) + 6 (code) + 31*f32 + 2*u16 + 2*u32 = 145 bytes
    ensure_bytes!(buf, 145);

    // Skip first 2 bytes (count)
    buf.advance(2);

    // Market (1 byte) + Code (6 bytes)
    let market = buf.get_u8();
    let mut code_bytes = [0u8; 6];
    buf.copy_to_slice(&mut code_bytes);

    // Read remaining fields according to format:
    // <fHHIIffffffffffffffffffffffffffffff
    // f = float (4 bytes)
    // H = unsigned short (2 bytes)
    // I = unsigned int (4 bytes)
    // Total: 4 + 2 + 2 + 4 + 4 + 4*4 + 4*2 + 4*30 + 4*2 = ~168 bytes

    let floating_shares = buf.get_f32_le();
    let province = buf.get_u16_le();
    let industry = buf.get_u16_le();
    let updated_date = buf.get_u32_le();
    let ipo_date = buf.get_u32_le();

    // Equity structure
    let total_shares = buf.get_f32_le();
    let state_shares = buf.get_f32_le();
    let promoter_legal_shares = buf.get_f32_le();
    let legal_shares = buf.get_f32_le();
    let b_shares = buf.get_f32_le();
    let h_shares = buf.get_f32_le();
    let employee_shares = buf.get_f32_le();

    // Assets
    let total_assets = buf.get_f32_le();
    let current_assets = buf.get_f32_le();
    let fixed_assets = buf.get_f32_le();
    let intangible_assets = buf.get_f32_le();

    // Shareholders and liabilities
    let shareholder_count = buf.get_f32_le();
    let current_liabilities = buf.get_f32_le();
    let long_term_liabilities = buf.get_f32_le();
    let capital_reserve = buf.get_f32_le();
    let net_assets = buf.get_f32_le();

    // Income statement
    let main_revenue = buf.get_f32_le();
    let main_profit = buf.get_f32_le();
    let accounts_receivable = buf.get_f32_le();
    let operating_profit = buf.get_f32_le();
    let investment_income = buf.get_f32_le();

    // Cash flow
    let operating_cash_flow = buf.get_f32_le();
    let total_cash_flow = buf.get_f32_le();

    // Other metrics
    let inventory = buf.get_f32_le();
    let total_profit = buf.get_f32_le();
    let after_tax_profit = buf.get_f32_le();
    let net_profit = buf.get_f32_le();
    let retained_earnings = buf.get_f32_le();
    let net_assets_per_share = buf.get_f32_le();
    let reserved2 = buf.get_f32_le();

    // Decode code to string
    let code = String::from_utf8_lossy(&code_bytes)
        .trim_end_matches('\0')
        .to_string();

    Ok(FinanceInfo {
        market,
        code,
        province,
        industry,
        updated_date,
        ipo_date,
        floating_shares: floating_shares as f64 * 10000.0,
        total_shares: total_shares as f64 * 10000.0,
        state_shares: state_shares as f64 * 10000.0,
        promoter_legal_shares: promoter_legal_shares as f64 * 10000.0,
        legal_shares: legal_shares as f64 * 10000.0,
        b_shares: b_shares as f64 * 10000.0,
        h_shares: h_shares as f64 * 10000.0,
        employee_shares: employee_shares as f64 * 10000.0,
        total_assets: total_assets as f64 * 10000.0,
        current_assets: current_assets as f64 * 10000.0,
        fixed_assets: fixed_assets as f64 * 10000.0,
        intangible_assets: intangible_assets as f64 * 10000.0,
        shareholder_count: shareholder_count as f64,
        current_liabilities: current_liabilities as f64 * 10000.0,
        long_term_liabilities: long_term_liabilities as f64 * 10000.0,
        capital_reserve: capital_reserve as f64 * 10000.0,
        net_assets: net_assets as f64 * 10000.0,
        main_revenue: main_revenue as f64 * 10000.0,
        main_profit: main_profit as f64 * 10000.0,
        accounts_receivable: accounts_receivable as f64 * 10000.0,
        operating_profit: operating_profit as f64 * 10000.0,
        investment_income: investment_income as f64 * 10000.0,
        operating_cash_flow: operating_cash_flow as f64 * 10000.0,
        total_cash_flow: total_cash_flow as f64 * 10000.0,
        inventory: inventory as f64 * 10000.0,
        total_profit: total_profit as f64 * 10000.0,
        after_tax_profit: after_tax_profit as f64 * 10000.0,
        net_profit: net_profit as f64 * 10000.0,
        retained_earnings: retained_earnings as f64 * 10000.0,
        net_assets_per_share: net_assets_per_share as f64,
        reserved2: reserved2 as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_date() {
        assert_eq!(format_date(20240424), "2024-04-24");
        assert_eq!(format_date(19910403), "1991-04-03");
        assert_eq!(format_date(0), "0000-00-00");
    }

    #[test]
    fn test_finance_info_market_name() {
        let info = FinanceInfo {
            market: 0,
            code: "000001".to_string(),
            province: 0,
            industry: 0,
            updated_date: 0,
            ipo_date: 0,
            floating_shares: 0.0,
            total_shares: 0.0,
            state_shares: 0.0,
            promoter_legal_shares: 0.0,
            legal_shares: 0.0,
            b_shares: 0.0,
            h_shares: 0.0,
            employee_shares: 0.0,
            total_assets: 0.0,
            current_assets: 0.0,
            fixed_assets: 0.0,
            intangible_assets: 0.0,
            shareholder_count: 0.0,
            current_liabilities: 0.0,
            long_term_liabilities: 0.0,
            capital_reserve: 0.0,
            net_assets: 0.0,
            main_revenue: 0.0,
            main_profit: 0.0,
            accounts_receivable: 0.0,
            operating_profit: 0.0,
            investment_income: 0.0,
            operating_cash_flow: 0.0,
            total_cash_flow: 0.0,
            inventory: 0.0,
            total_profit: 0.0,
            after_tax_profit: 0.0,
            net_profit: 0.0,
            retained_earnings: 0.0,
            net_assets_per_share: 0.0,
            reserved2: 0.0,
        };

        assert_eq!(info.market_name(), "深圳");

        let info_sh = FinanceInfo { market: 1, ..info };
        assert_eq!(info_sh.market_name(), "上海");
    }

    #[test]
    fn test_format_date_edge_cases() {
        assert_eq!(format_date(20240101), "2024-01-01");
        assert_eq!(format_date(19991231), "1999-12-31");
    }

    #[test]
    fn test_ipo_date_str() {
        let info = FinanceInfo {
            market: 1,
            code: "600000".to_string(),
            province: 0,
            industry: 0,
            updated_date: 0,
            ipo_date: 20100101,
            floating_shares: 0.0,
            total_shares: 0.0,
            state_shares: 0.0,
            promoter_legal_shares: 0.0,
            legal_shares: 0.0,
            b_shares: 0.0,
            h_shares: 0.0,
            employee_shares: 0.0,
            total_assets: 0.0,
            current_assets: 0.0,
            fixed_assets: 0.0,
            intangible_assets: 0.0,
            shareholder_count: 0.0,
            current_liabilities: 0.0,
            long_term_liabilities: 0.0,
            capital_reserve: 0.0,
            net_assets: 0.0,
            main_revenue: 0.0,
            main_profit: 0.0,
            accounts_receivable: 0.0,
            operating_profit: 0.0,
            investment_income: 0.0,
            operating_cash_flow: 0.0,
            total_cash_flow: 0.0,
            inventory: 0.0,
            total_profit: 0.0,
            after_tax_profit: 0.0,
            net_profit: 0.0,
            retained_earnings: 0.0,
            net_assets_per_share: 0.0,
            reserved2: 0.0,
        };
        assert_eq!(info.ipo_date_str(), "2010-01-01");
    }

    #[test]
    fn test_updated_date_str() {
        let info = FinanceInfo {
            market: 1,
            code: "600000".to_string(),
            province: 0,
            industry: 0,
            updated_date: 20240424,
            ipo_date: 0,
            floating_shares: 0.0,
            total_shares: 0.0,
            state_shares: 0.0,
            promoter_legal_shares: 0.0,
            legal_shares: 0.0,
            b_shares: 0.0,
            h_shares: 0.0,
            employee_shares: 0.0,
            total_assets: 0.0,
            current_assets: 0.0,
            fixed_assets: 0.0,
            intangible_assets: 0.0,
            shareholder_count: 0.0,
            current_liabilities: 0.0,
            long_term_liabilities: 0.0,
            capital_reserve: 0.0,
            net_assets: 0.0,
            main_revenue: 0.0,
            main_profit: 0.0,
            accounts_receivable: 0.0,
            operating_profit: 0.0,
            investment_income: 0.0,
            operating_cash_flow: 0.0,
            total_cash_flow: 0.0,
            inventory: 0.0,
            total_profit: 0.0,
            after_tax_profit: 0.0,
            net_profit: 0.0,
            retained_earnings: 0.0,
            net_assets_per_share: 0.0,
            reserved2: 0.0,
        };
        assert_eq!(info.updated_date_str(), "2024-04-24");
    }

    #[test]
    fn test_parse_finance_info_basic() {
        // Build a minimal finance info response body
        // count(2) + market(1) + code(6) + fields...
        let mut body = vec![0x01, 0x00]; // count = 1
        body.push(1); // market = Shanghai
        body.extend_from_slice(b"600000"); // code

        // floating_shares (f32) + province (u16) + industry (u16)
        body.extend_from_slice(&1000.0f32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());

        // updated_date + ipo_date
        body.extend_from_slice(&20240424u32.to_le_bytes());
        body.extend_from_slice(&20100101u32.to_le_bytes());

        // 30 more f32 fields (total 31 f32 fields after floating_shares)
        for _ in 0..30 {
            body.extend_from_slice(&0.0f32.to_le_bytes());
        }

        let result = parse_finance_info(&body).unwrap();
        assert_eq!(result.market, 1);
        assert_eq!(result.code, "600000");
        assert_eq!(result.province, 1);
        assert_eq!(result.industry, 2);
        assert_eq!(result.updated_date_str(), "2024-04-24");
        assert_eq!(result.ipo_date_str(), "2010-01-01");
        assert_eq!(result.floating_shares, 1000.0f64 * 10000.0);
    }

    #[test]
    fn test_parse_finance_info_buffer_too_short() {
        let body = [0x01, 0x00, 0x01];
        let result = parse_finance_info(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_finance_info_market_name_unknown() {
        let info = FinanceInfo {
            market: 255,
            code: "TEST".to_string(),
            province: 0,
            industry: 0,
            updated_date: 0,
            ipo_date: 0,
            floating_shares: 0.0,
            total_shares: 0.0,
            state_shares: 0.0,
            promoter_legal_shares: 0.0,
            legal_shares: 0.0,
            b_shares: 0.0,
            h_shares: 0.0,
            employee_shares: 0.0,
            total_assets: 0.0,
            current_assets: 0.0,
            fixed_assets: 0.0,
            intangible_assets: 0.0,
            shareholder_count: 0.0,
            current_liabilities: 0.0,
            long_term_liabilities: 0.0,
            capital_reserve: 0.0,
            net_assets: 0.0,
            main_revenue: 0.0,
            main_profit: 0.0,
            accounts_receivable: 0.0,
            operating_profit: 0.0,
            investment_income: 0.0,
            operating_cash_flow: 0.0,
            total_cash_flow: 0.0,
            inventory: 0.0,
            total_profit: 0.0,
            after_tax_profit: 0.0,
            net_profit: 0.0,
            retained_earnings: 0.0,
            net_assets_per_share: 0.0,
            reserved2: 0.0,
        };
        assert_eq!(info.market_name(), "未知");
    }

    #[test]
    fn test_format_date_single_digit() {
        assert_eq!(format_date(20240101), "2024-01-01");
        assert_eq!(format_date(20241201), "2024-12-01");
    }
}
