use async_trait::async_trait;
use crate::models::{SandboxRequest, SandboxResult};
use crate::sandbox::executor::{SandboxError, SandboxExecutor};

/// Docker-based sandbox executor (skeleton).
/// Real implementation should use bollard or docker CLI with strict defaults:
/// - network: none or explicit allowlist
/// - read-only root + controlled mounts
/// - cpu/memory/pids limits
/// - no privileged, no docker.sock
pub struct DockerSandboxExecutor {
    // docker client handle would go here
}

impl DockerSandboxExecutor {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl SandboxExecutor for DockerSandboxExecutor {
    async fn execute(&self, request: SandboxRequest) -> Result<SandboxResult, SandboxError> {
        // TODO: implement with bollard
        // 1. Create container with limits & mounts from request
        // 2. Start and wait with timeout
        // 3. Collect logs & artifacts
        // 4. Remove container
        Err(SandboxError::Execution(
            "DockerSandboxExecutor not yet implemented".into(),
        ))
    }
}
