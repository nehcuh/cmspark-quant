use async_trait::async_trait;
use crate::models::{SandboxRequest, SandboxResult};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("timeout")]
    Timeout,
    #[error("resource limit exceeded")]
    ResourceLimit,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Unified sandbox execution interface.
/// Default implementation should be Docker; process-isolation is optional for dev.
#[async_trait]
pub trait SandboxExecutor: Send + Sync {
    async fn execute(&self, request: SandboxRequest) -> Result<SandboxResult, SandboxError>;

    /// Optional: stream logs while running (for Dashboard).
    async fn execute_with_logs(
        &self,
        request: SandboxRequest,
        _log_callback: impl Fn(String) + Send + 'static,
    ) -> Result<SandboxResult, SandboxError> {
        self.execute(request).await
    }
}
