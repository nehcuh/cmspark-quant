//! High-level query API with date range support.

pub mod api;

#[cfg(feature = "dataframe")]
pub mod dataframe;

pub use api::{DataSource, QueryApi, QueryOptions};

#[cfg(feature = "dataframe")]
pub use dataframe::{bars_to_dataframe, quotes_to_dataframe};

use serde::{Deserialize, Serialize};

/// Query result with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult<T> {
    pub data: Vec<T>,
    pub start_date: String,
    pub end_date: String,
    pub total_count: usize,
}

/// Bar data for K-line queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BarData {
    pub datetime: String,
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub amount: f64,
}

impl From<crate::commands::Kline> for BarData {
    fn from(kline: crate::commands::Kline) -> Self {
        let datetime = kline.datetime();
        let date = datetime[..10].to_string();
        Self {
            datetime,
            date: date.clone(),
            open: kline.open,
            high: kline.high,
            low: kline.low,
            close: kline.close,
            volume: kline.volume,
            amount: kline.amount,
        }
    }
}

/// Quote data for real-time queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteData {
    pub code: String,
    pub market: u8,
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
    pub change_pct: f64,
}

impl QuoteData {
    pub fn from_quote_with_code(quote: &crate::commands::SecurityQuote, code: &str) -> Self {
        let change_pct = if quote.last_close > 0.0 {
            (quote.price - quote.last_close) / quote.last_close * 100.0
        } else {
            0.0
        };

        Self {
            code: code.to_string(),
            market: quote.market,
            price: quote.price,
            last_close: quote.last_close,
            open: quote.open,
            high: quote.high,
            low: quote.low,
            volume: quote.volume,
            amount: quote.amount,
            bid1: quote.bid1,
            ask1: quote.ask1,
            bid_vol1: quote.bid_vol1,
            ask_vol1: quote.ask_vol1,
            change_pct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_result_creation() {
        let result = QueryResult {
            data: vec![BarData {
                datetime: "2024-01-01 15:00".to_string(),
                date: "2024-01-01".to_string(),
                open: 10.0,
                high: 11.0,
                low: 9.5,
                close: 10.5,
                volume: 1000.0,
                amount: 10000.0,
            }],
            start_date: "2024-01-01".to_string(),
            end_date: "2024-01-01".to_string(),
            total_count: 1,
        };
        assert_eq!(result.total_count, 1);
    }

    #[test]
    fn test_bardata_default_fields() {
        let bar = BarData {
            datetime: "2024-01-01 15:00".to_string(),
            date: "2024-01-01".to_string(),
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume: 0.0,
            amount: 0.0,
        };
        assert_eq!(bar.date, "2024-01-01");
    }

    #[test]
    fn test_quotedata_serialization() {
        let quote = QuoteData {
            code: "600000".to_string(),
            market: 1,
            price: 10.5,
            last_close: 10.0,
            open: 10.0,
            high: 11.0,
            low: 9.5,
            volume: 1000.0,
            amount: 10000.0,
            bid1: 10.4,
            ask1: 10.6,
            bid_vol1: 100.0,
            ask_vol1: 200.0,
            change_pct: 5.0,
        };
        let json = serde_json::to_string(&quote).unwrap();
        assert!(json.contains("600000"));
    }
}
