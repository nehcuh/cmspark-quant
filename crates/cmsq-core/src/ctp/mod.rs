//! P3: CTP market data (observe-only). See `md.rs`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tick {
    pub instrument: String,
    /// RFC3339 exchange timestamp (Asia/Shanghai offset)
    pub ts: String,
    pub trading_day: String,
    pub last: f64,
    pub volume: f64,
    pub open_interest: Option<f64>,
    pub bid1: Option<f64>,
    pub ask1: Option<f64>,
    pub source: String,
}

mod md;
pub use md::*;
#[cfg(feature = "ctp")]
mod live;
