//! Sandbox execution interface. Docker is the default/recommended backend; the
//! bubblewrap process backend exists for dev / low-risk use and is weaker.
pub mod docker;
pub mod process;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxLimits {
    pub cpu_cores: f64,
    pub memory_mb: u64,
    pub pids: i64,
    pub timeout_secs: u64,
    /// Empty = deny all network (the only mode supported in P0).
    pub network_allowlist: Vec<String>,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            cpu_cores: 1.0,
            memory_mb: 512,
            pids: 128,
            timeout_secs: 120,
            network_allowlist: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    pub host: PathBuf,
    pub container: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRequest {
    pub id: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub limits: SandboxLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Outcome {
    Exited,
    TimedOut,
    OomKilled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxResult {
    pub id: String,
    pub outcome: Outcome,
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub policy: serde_json::Value,
}

#[async_trait]
pub trait SandboxExecutor: Send + Sync {
    fn name(&self) -> &'static str;
    /// Is this a hardened backend acceptable for untrusted code?
    fn hardened(&self) -> bool;
    async fn execute(&self, req: &SandboxRequest) -> Result<SandboxResult, SandboxError>;
    /// Identity of the runner as seen by the BACKEND (never by the container):
    /// docker: "<image id>|cmsq.runner.version=<label>"; dev: "dev-process|sha256:<script>".
    async fn runner_identity(&self, image: &str) -> Option<String>;
}

/// Per-stream log capture cap; a stream that reaches it is reported as `output_cap`.
pub const OUTPUT_CAP: usize = 1024 * 1024;

/// Policy checks shared by all backends (defence in depth: callers cannot ask for
/// network, the docker socket, writable data, or relative/odd mount paths).
pub fn validate(req: &SandboxRequest) -> Result<(), SandboxError> {
    if !req.limits.network_allowlist.is_empty() {
        return Err(SandboxError::InvalidRequest(
            "network allowlists are not supported in P0; network is always denied".into(),
        ));
    }
    if req.limits.memory_mb < 64
        || req.limits.timeout_secs == 0
        || req.limits.cpu_cores <= 0.0
        || req.limits.pids <= 0
    {
        return Err(SandboxError::InvalidRequest(
            "resource limits must be set".into(),
        ));
    }
    let mut writable = 0;
    for m in &req.mounts {
        let hs = m.host.to_string_lossy();
        if !m.host.is_absolute()
            || hs.contains("..")
            || !m.container.starts_with('/')
            || m.container.contains("..")
        {
            return Err(SandboxError::InvalidRequest(format!(
                "mount paths must be absolute and normalised: {hs}"
            )));
        }
        if hs.contains("docker.sock")
            || hs.ends_with("/run")
            || hs == "/"
            || hs.starts_with("/var/run")
            || hs.starts_with("/proc")
            || hs.starts_with("/sys")
            || hs.starts_with("/dev")
        {
            return Err(SandboxError::InvalidRequest(format!(
                "forbidden host mount: {hs}"
            )));
        }
        if !m.read_only {
            writable += 1;
            if m.container != "/out" {
                return Err(SandboxError::InvalidRequest(
                    "only /out may be writable".into(),
                ));
            }
        }
    }
    if writable > 1 {
        return Err(SandboxError::InvalidRequest(
            "at most one writable mount".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn req() -> SandboxRequest {
        SandboxRequest {
            id: "t".into(),
            image: "x".into(),
            command: vec!["true".into()],
            env: vec![],
            mounts: vec![
                Mount {
                    host: "/tmp/d".into(),
                    container: "/data".into(),
                    read_only: true,
                },
                Mount {
                    host: "/tmp/o".into(),
                    container: "/out".into(),
                    read_only: false,
                },
            ],
            limits: SandboxLimits::default(),
        }
    }
    #[test]
    fn default_request_ok() {
        validate(&req()).unwrap();
    }
    #[test]
    fn rejects_socket_network_and_writable_data() {
        let mut r = req();
        r.mounts.push(Mount {
            host: "/var/run/docker.sock".into(),
            container: "/s".into(),
            read_only: true,
        });
        assert!(validate(&r).is_err());
        let mut r = req();
        r.limits.network_allowlist = vec!["example.com".into()];
        assert!(validate(&r).is_err());
        let mut r = req();
        r.mounts[0].read_only = false;
        assert!(validate(&r).is_err());
        let mut r = req();
        r.mounts[0].host = "/tmp/../etc".into();
        assert!(validate(&r).is_err());
    }
}
