use crate::models::{Strategy, StrategyStatus, AuditEvent};
use uuid::Uuid;
use chrono::Utc;

/// Enforces valid transitions and emits audit events.
pub struct StrategyStateMachine;

impl StrategyStateMachine {
    pub fn transition(
        strategy: &mut Strategy,
        to: StrategyStatus,
        actor: &str,
        details: serde_json::Value,
    ) -> Result<AuditEvent, String> {
        let from = strategy.status.clone();
        if !Self::is_valid_transition(&from, &to) {
            return Err(format!("invalid transition {:?} -> {:?}", from, to));
        }

        strategy.status = to.clone();
        strategy.updated_at = Utc::now();

        Ok(AuditEvent {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            actor: actor.to_string(),
            action: format!("transition:{:?}->{:?}", from, to),
            target_id: Some(strategy.id),
            details,
            success: true,
        })
    }

    fn is_valid_transition(from: &StrategyStatus, to: &StrategyStatus) -> bool {
        matches!(
            (from, to),
            (StrategyStatus::Ingested, StrategyStatus::Extracted)
                | (StrategyStatus::Extracted, StrategyStatus::Backtesting)
                | (StrategyStatus::Backtesting, StrategyStatus::Evaluated)
                | (StrategyStatus::Backtesting, StrategyStatus::Failed)
                | (StrategyStatus::Evaluated, StrategyStatus::Promoted)
                | (StrategyStatus::Evaluated, StrategyStatus::Rejected)
        )
    }
}
