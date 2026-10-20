use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Explicit strategy lifecycle states.
/// All transitions must be recorded in the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrategyStatus {
    Ingested,
    Extracted,
    Backtesting,
    Evaluated,
    Promoted,
    Rejected,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strategy {
    pub id: Uuid,
    pub source: String,
    pub source_url: Option<String>,
    pub title: String,
    pub raw_content_path: String,
    pub extracted: Option<ExtractedStrategy>,
    pub status: StrategyStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedStrategy {
    pub summary: String,
    pub assumptions: Vec<String>,
    pub parameters: serde_json::Value,
    pub code_snippet: Option<String>,
    pub risks: Vec<String>,
    pub extracted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestResult {
    pub strategy_id: Uuid,
    pub metrics: BacktestMetrics,
    pub data_range: String,
    pub sample_split: String,
    pub cost_model: String,
    pub point_in_time: bool,
    pub artifact_path: String,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestMetrics {
    pub total_return: f64,
    pub sharpe: f64,
    pub max_drawdown: f64,
    pub turnover: f64,
    pub oos_return: Option<f64>,
    pub oos_sharpe: Option<f64>,
}
