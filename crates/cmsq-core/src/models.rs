use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Explicit strategy lifecycle states. All transitions are audited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum StrategyStatus {
    Ingested,
    Extracted,
    Backtesting,
    Evaluated,
    GatePassed,
    Promoted,
    Rejected,
    Failed,
}

impl StrategyStatus {
    pub const ALL: [StrategyStatus; 8] = [
        Self::Ingested,
        Self::Extracted,
        Self::Backtesting,
        Self::Evaluated,
        Self::GatePassed,
        Self::Promoted,
        Self::Rejected,
        Self::Failed,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ingested => "Ingested",
            Self::Extracted => "Extracted",
            Self::Backtesting => "Backtesting",
            Self::Evaluated => "Evaluated",
            Self::GatePassed => "GatePassed",
            Self::Promoted => "Promoted",
            Self::Rejected => "Rejected",
            Self::Failed => "Failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.as_str().eq_ignore_ascii_case(s))
    }
}

impl fmt::Display for StrategyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strategy {
    pub id: String,
    pub title: String,
    pub source_site: String,
    pub source_url: Option<String>,
    pub author: Option<String>,
    pub scraped_at: Option<String>,
    pub ingested_at: DateTime<Utc>,
    pub raw_path: String,
    pub content_sha256: String,
    pub sample: bool,
    pub status: StrategyStatus,
    pub attempts: i64,
    pub last_reason: Option<String>,
    pub extracted: Option<ExtractedStrategy>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractedStrategy {
    pub extractor: String,
    pub summary: String,
    pub assumptions: Vec<String>,
    pub parameters: serde_json::Map<String, serde_json::Value>,
    pub code: Option<String>,
    pub risks: Vec<Risk>,
    pub extracted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Risk {
    pub code: String,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub strategy_id: String,
    pub backend: String,
    pub image: String,
    pub symbol: String,
    pub data_source: String,
    pub status: String,
    pub exit_code: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub job_dir: String,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
    /// PID of the cmsq process that owns the job (crash recovery)
    #[serde(default)]
    pub owner_pid: Option<i64>,
    /// Identity of the runner as reported by the BACKEND (docker image id + label),
    /// never by the container.
    #[serde(default)]
    pub runner_identity: Option<String>,
    /// "dev" (development-window evaluation) or "vault" (final holdout at promote)
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluation {
    pub id: String,
    pub strategy_id: String,
    pub job_id: String,
    pub passed: bool,
    pub reasons: Vec<String>,
    pub checks: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub strategy_id: String,
    pub job_id: String,
    pub symbol: String,
    pub date: String,
    pub target_position: f64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub seq: i64,
    pub ts: String,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub details: serde_json::Value,
    pub success: bool,
    pub prev_hash: String,
    pub hash: String,
}
