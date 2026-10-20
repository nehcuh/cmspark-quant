//! Typed errors for the orchestrator's public API. They travel inside
//! `anyhow::Error`, so callers (CLI, dashboard, tests) can `downcast_ref::<CoreError>()`
//! to branch on the kind instead of matching strings.
use crate::models::StrategyStatus;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum CoreError {
    #[error("strategy not found: {0}")]
    NotFound(String),
    #[error("invalid transition {from} -> {to}")]
    InvalidTransition { from: StrategyStatus, to: StrategyStatus },
    #[error("confirmation mismatch: type exactly {expected:?}")]
    ConfirmationMismatch { expected: String },
    #[error("cannot promote {id}: status is {status}, promotion requires GatePassed")]
    NotGatePassed { id: String, status: StrategyStatus },
    #[error("strategy {0} has no extracted code")]
    NoCode(String),
    #[error("no market data for {0}; run `cmsq data fetch {0}`")]
    NoData(String),
    #[error("sandbox backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("runner image mismatch: {0}")]
    RunnerMismatch(String),
    #[error("audit log integrity failure: {0}")]
    AuditIntegrity(String),
}
