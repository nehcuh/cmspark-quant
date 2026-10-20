use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Append-only audit event. All critical actions must emit one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub actor: String,          // "system", "user", "sandbox", etc.
    pub action: String,         // "ingest", "extract", "start_backtest", "promote", ...
    pub target_id: Option<Uuid>,
    pub details: serde_json::Value,
    pub success: bool,
}
