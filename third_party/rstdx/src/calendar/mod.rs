#![allow(clippy::module_inception, clippy::should_implement_trait)]

//! Trading calendar module for managing trading dates.

mod calendar;
mod dates;
mod tushare;

pub use calendar::TradingCalendar;
pub use dates::{NormalizeDirection, TradeDate, parse_date_flexible, date_to_int, int_to_date};
pub use tushare::{TradeCalCache, TushareClient};

use serde::{Deserialize, Serialize};

/// Market type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Market {
    Shanghai,
    Shenzhen,
    Beijing,
}

impl Market {
    pub fn from_code(code: &str) -> Self {
        match code {
            "1" | "sh" | "SH" => Market::Shanghai,
            "0" | "sz" | "SZ" => Market::Shenzhen,
            "2" | "bj" | "BJ" => Market::Beijing,
            _ => {
                // Auto-detect from stock code prefix.
                //
                // Shanghai: 6xxxxx (A-shares), 5xxxxx (funds), 688xxx (STAR),
                //            900xxx (B-shares)
                // Shenzhen: 0xxxxx/1xxxxx/2xxxxx/3xxxxx (all others)
                // Beijing:  4xxxxx/8xxxxx/9xxxxx (NEEQ / 北交所)
                //
                // NOTE: 000001-000999 are ambiguous — they can be Shanghai
                // indices (e.g. 000001 上证指数) or Shenzhen stocks (e.g.
                // 000001 平安银行). Callers must pass an explicit market code
                // when querying Shanghai indices in this range.
                if code.starts_with('6')
                    || code.starts_with('5')
                    || code.starts_with("688")
                    || code.starts_with("900")
                {
                    Market::Shanghai
                } else if code.starts_with('4')
                    || code.starts_with('8')
                    || code.starts_with('9')
                {
                    Market::Beijing
                } else {
                    Market::Shenzhen
                }
            }
        }
    }

    pub fn to_tdx_code(&self) -> u8 {
        match self {
            Market::Shanghai => 1,
            Market::Shenzhen => 0,
            Market::Beijing => 2,
        }
    }
}

/// Frequency type for K-line data
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frequency {
    OneMin,
    FiveMin,
    FifteenMin,
    ThirtyMin,
    SixtyMin,
    Day,
    Week,
    Month,
}

impl Frequency {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "1min" | "1m" | "1" => Frequency::OneMin,
            "5min" | "5m" | "5" => Frequency::FiveMin,
            "15min" | "15m" | "15" => Frequency::FifteenMin,
            "30min" | "30m" | "30" => Frequency::ThirtyMin,
            "60min" | "60m" | "60" | "1h" => Frequency::SixtyMin,
            "day" | "d" | "daily" => Frequency::Day,
            "week" | "w" | "weekly" => Frequency::Week,
            "month" | "M" | "monthly" => Frequency::Month,
            _ => Frequency::Day,
        }
    }

    pub fn to_tdx_code(&self) -> u16 {
        match self {
            Frequency::OneMin => 8,
            Frequency::FiveMin => 0,
            Frequency::FifteenMin => 1,
            Frequency::ThirtyMin => 2,
            Frequency::SixtyMin => 3,
            Frequency::Day => 4, // Daily K-line
            Frequency::Week => 5,
            Frequency::Month => 6,
        }
    }

    /// Get the multiplier for calculating number of records needed
    /// (TDX returns fixed number of bars per request)
    pub fn get_multiplier(&self) -> u32 {
        match self {
            Frequency::OneMin => 240,
            Frequency::FiveMin => 48,
            Frequency::FifteenMin => 16,
            Frequency::ThirtyMin => 8,
            Frequency::SixtyMin => 4,
            Frequency::Day => 1,
            Frequency::Week => 1,
            Frequency::Month => 1,
        }
    }
}

/// Adjustment type for price data
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdjustType {
    None,
    Qfq, // Forward adjustment
    Hfq, // Backward adjustment
}

impl AdjustType {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "qfq" | "01" | "forward" => AdjustType::Qfq,
            "hfq" | "02" | "backward" => AdjustType::Hfq,
            _ => AdjustType::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_market_from_code() {
        assert_eq!(Market::from_code("1"), Market::Shanghai);
        assert_eq!(Market::from_code("0"), Market::Shenzhen);
        assert_eq!(Market::from_code("2"), Market::Beijing);
        assert_eq!(Market::from_code("600000"), Market::Shanghai);
        assert_eq!(Market::from_code("688001"), Market::Shanghai);
        assert_eq!(Market::from_code("900901"), Market::Shanghai);
        assert_eq!(Market::from_code("430047"), Market::Beijing);
        assert_eq!(Market::from_code("835185"), Market::Beijing);
        assert_eq!(Market::from_code("872925"), Market::Beijing);
        // 000001 is ambiguous (could be 上证指数 SH or 平安银行 SZ).
        // Stock takes precedence in auto-detect since most 0xxxxx codes are SZ stocks.
        assert_eq!(Market::from_code("000001"), Market::Shenzhen);
    }

    #[test]
    fn test_frequency_from_str() {
        assert_eq!(Frequency::from_str("1min"), Frequency::OneMin);
        assert_eq!(Frequency::from_str("5min"), Frequency::FiveMin);
        assert_eq!(Frequency::from_str("day"), Frequency::Day);
        assert_eq!(Frequency::from_str("1h"), Frequency::SixtyMin);
    }

    #[test]
    fn test_adjust_type_from_str() {
        assert_eq!(AdjustType::from_str("qfq"), AdjustType::Qfq);
        assert_eq!(AdjustType::from_str("hfq"), AdjustType::Hfq);
        assert_eq!(AdjustType::from_str("none"), AdjustType::None);
    }

    #[test]
    fn test_frequency_to_tdx_code() {
        assert_eq!(Frequency::Day.to_tdx_code(), 4);
        assert_eq!(Frequency::Week.to_tdx_code(), 5);
        assert_eq!(Frequency::Month.to_tdx_code(), 6);
        assert_eq!(Frequency::OneMin.to_tdx_code(), 8);
    }

    #[test]
    fn test_frequency_get_multiplier() {
        assert_eq!(Frequency::Day.get_multiplier(), 1);
        assert_eq!(Frequency::OneMin.get_multiplier(), 240);
        assert_eq!(Frequency::FiveMin.get_multiplier(), 48);
    }
}
