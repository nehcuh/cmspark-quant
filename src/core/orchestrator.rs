use crate::models::{Strategy, SandboxRequest, AuditEvent};
use crate::sandbox::SandboxExecutor;
use std::sync::Arc;

/// Central coordinator.
/// Owns state transitions, audit emission, and sandbox dispatch.
pub struct Orchestrator<E: SandboxExecutor> {
    pub executor: Arc<E>,
    // db / store handles would be injected here
}

impl<E: SandboxExecutor> Orchestrator<E> {
    pub fn new(executor: Arc<E>) -> Self {
        Self { executor }
    }

    /// Example: submit a strategy for backtesting.
    pub async fn submit_backtest(
        &self,
        strategy: &Strategy,
        request: SandboxRequest,
    ) -> Result<AuditEvent, String> {
        // 1. transition to Backtesting
        // 2. call executor.execute(request)
        // 3. persist result + transition to Evaluated / Failed
        // 4. return audit event
        let _ = self.executor.execute(request).await;
        Ok(AuditEvent {
            id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor: "orchestrator".into(),
            action: "submit_backtest".into(),
            target_id: Some(strategy.id),
            details: serde_json::json!({ "status": "dispatched" }),
            success: true,
        })
    }
}
