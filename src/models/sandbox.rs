use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxLimits {
    pub cpu_cores: f32,
    pub memory_mb: u64,
    pub timeout_secs: u64,
    pub network_allowlist: Vec<String>, // empty = deny all
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            cpu_cores: 1.0,
            memory_mb: 512,
            timeout_secs: 300,
            network_allowlist: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRequest {
    pub id: Uuid,
    pub image: String,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub read_only_mounts: Vec<(String, String)>, // (host_path, container_path)
    pub writable_mounts: Vec<(String, String)>,
    pub limits: SandboxLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxResult {
    pub id: Uuid,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub artifact_paths: Vec<String>,
    pub duration_ms: u64,
}
