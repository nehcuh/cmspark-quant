//! Trade date structures and utilities.

use crate::error::{Error, Result};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

/// Represents a single trading date with metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeDate {
    pub date: NaiveDate,
    pub is_trading: bool,
    pub reason: Option<String>,
}

impl TradeDate {
    pub fn new(date: NaiveDate, is_trading: bool) -> Self {
        Self {
            date,
            is_trading,
            reason: None,
        }
    }

    pub fn with_reason(mut self, reason: String) -> Self {
        self.reason = Some(reason);
        self
    }

    pub fn from_str(date_str: &str) -> std::result::Result<Self, String> {
        let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
            .map_err(|e| format!("Invalid date format: {}", e))?;
        Ok(Self::new(date, true))
    }
}

/// Direction for normalizing a non-trading date to the nearest trading date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizeDirection {
    /// Move backward to the previous trading date.
    Previous,
    /// Move forward to the next trading date.
    Next,
    /// Prefer the nearest trading date (ties: previous).
    Nearest,
}

/// Calculate trading days needed for TDX query based on date range.
#[allow(dead_code)]
pub fn calculate_tdx_request_count(start_date: NaiveDate, end_date: NaiveDate) -> u32 {
    let days = (end_date - start_date).num_days().max(0) as u32;
    // Add buffer for weekends and holidays (approximately 30% non-trading days)
    (days as f32 * 1.5) as u32
}

/// Parse date string to NaiveDate (YYYY-MM-DD format).
#[allow(dead_code)]
pub fn parse_date(date_str: &str) -> std::result::Result<NaiveDate, String> {
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .map_err(|e| format!("Invalid date format '{}': {}", date_str, e))
}

/// Flexible date parser supporting multiple formats.
///
/// Supported formats:
/// - `"2024-01-26"` (ISO 8601)
/// - `"20240126"` (compact, no separators)
pub fn parse_date_flexible(input: &str) -> Result<NaiveDate> {
    let trimmed = input.trim();

    // Try YYYY-MM-DD (strict 4-digit year)
    if trimmed.len() == 10
        && trimmed.matches('-').count() == 2
        && let Ok(d) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
    {
        return Ok(d);
    }

    // Try YYYYMMDD (strict 8 digits)
    if trimmed.len() == 8
        && trimmed.chars().all(|c| c.is_ascii_digit())
        && let Ok(d) = NaiveDate::parse_from_str(trimmed, "%Y%m%d")
    {
        return Ok(d);
    }

    Err(Error::InvalidResponse(format!(
        "Unsupported date format '{}'. Expected 'YYYY-MM-DD' or 'YYYYMMdd'",
        input
    )))
}

/// Convert NaiveDate to integer format YYYYMMDD.
pub fn date_to_int(date: NaiveDate) -> u32 {
    date.year() as u32 * 10_000 + date.month() * 100 + date.day()
}

/// Convert integer format YYYYMMDD to NaiveDate.
pub fn int_to_date(date_int: u32) -> Result<NaiveDate> {
    let year = (date_int / 10_000) as i32;
    let month = (date_int % 10_000) / 100;
    let day = date_int % 100;

    NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| Error::InvalidResponse(format!("Invalid date integer: {}", date_int)))
}

/// Format NaiveDate to string.
#[allow(dead_code)]
pub fn format_date(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trade_date_from_str() {
        let td = TradeDate::from_str("2024-01-01").unwrap();
        assert_eq!(td.date, NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
        assert!(td.is_trading);
    }

    #[test]
    fn test_parse_date() {
        let date = parse_date("2024-01-15").unwrap();
        assert_eq!(date, NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
    }

    #[test]
    fn test_parse_date_flexible_iso() {
        let d = parse_date_flexible("2024-01-26").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2024, 1, 26).unwrap());
    }

    #[test]
    fn test_parse_date_flexible_compact() {
        let d = parse_date_flexible("20240126").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2024, 1, 26).unwrap());
    }

    #[test]
    fn test_parse_date_flexible_invalid() {
        assert!(parse_date_flexible("not-a-date").is_err());
        assert!(parse_date_flexible("2024-13-01").is_err());
        assert!(parse_date_flexible("24-01-26").is_err());
    }

    #[test]
    fn test_date_to_int() {
        let d = NaiveDate::from_ymd_opt(2024, 1, 26).unwrap();
        assert_eq!(date_to_int(d), 20240126);
    }

    #[test]
    fn test_int_to_date() {
        let d = int_to_date(20240126).unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2024, 1, 26).unwrap());
    }

    #[test]
    fn test_int_to_date_invalid() {
        assert!(int_to_date(20241301).is_err());
        assert!(int_to_date(20240132).is_err());
    }

    #[test]
    fn test_format_date() {
        let date = NaiveDate::from_ymd_opt(2024, 6, 30).unwrap();
        assert_eq!(format_date(date), "2024-06-30");
    }

    #[test]
    fn test_calculate_tdx_request_count() {
        let start = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
        let count = calculate_tdx_request_count(start, end);
        assert!(count > 30); // Should be more than calendar days due to buffer
    }

    #[test]
    fn test_parse_date_invalid() {
        assert!(parse_date("not-a-date").is_err());
        assert!(parse_date("2024-13-01").is_err());
    }

    #[test]
    fn test_trade_date_new() {
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let td = TradeDate::new(date, true);
        assert_eq!(td.date, date);
        assert!(td.is_trading);
    }
}
