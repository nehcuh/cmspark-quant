use super::*;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

/// DEV backend: bubblewrap (all namespaces unshared incl. network) + prlimit.
/// Weaker than Docker (shared kernel attack surface, host python), so it reports
/// `hardened() == false` and the orchestrator labels every job with it.
/// Host filesystem exposure: only /usr (ro), /etc/ssl-free minimal /etc, and the
/// request's mounts.
pub struct ProcessSandboxExecutor;

impl ProcessSandboxExecutor {
    pub fn available() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    pub fn argv(req: &SandboxRequest) -> Vec<String> {
        let mem = req.limits.memory_mb * 1024 * 1024;
        let mut a: Vec<String> = vec![
            "prlimit".into(),
            format!("--as={mem}"),
            format!("--cpu={}", req.limits.timeout_secs),
            format!("--nproc={}", req.limits.pids * 8),
            "--".into(),
            "bwrap".into(),
            "--unshare-all".into(),
            "--die-with-parent".into(),
            "--new-session".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--ro-bind".into(),
            "/usr".into(),
            "/usr".into(),
            "--symlink".into(),
            "usr/lib".into(),
            "/lib".into(),
            "--symlink".into(),
            "usr/lib64".into(),
            "/lib64".into(),
            "--symlink".into(),
            "usr/bin".into(),
            "/bin".into(),
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--clearenv".into(),
            "--setenv".into(),
            "PATH".into(),
            "/usr/bin".into(),
            "--setenv".into(),
            "HOME".into(),
            "/tmp".into(),
            "--chdir".into(),
            "/tmp".into(),
        ];
        for m in &req.mounts {
            a.push(if m.read_only {
                "--ro-bind".into()
            } else {
                "--bind".into()
            });
            a.push(m.host.display().to_string());
            a.push(m.container.clone());
        }
        for (k, v) in &req.env {
            a.extend(["--setenv".into(), k.clone(), v.clone()]);
        }
        a.extend(req.command.iter().cloned());
        a
    }
}

#[async_trait]
impl SandboxExecutor for ProcessSandboxExecutor {
    fn name(&self) -> &'static str {
        "process-bwrap(dev)"
    }
    fn hardened(&self) -> bool {
        false
    }
    async fn runner_identity(&self, _image: &str) -> Option<String> {
        use sha2::{Digest, Sha256};
        Some(format!(
            "dev-process|sha256:{}",
            hex::encode(Sha256::digest(crate::home::RUNNER_PY.as_bytes()))
        ))
    }
    async fn execute(&self, req: &SandboxRequest) -> Result<SandboxResult, SandboxError> {
        validate(req)?;
        if !Self::available() {
            return Err(SandboxError::Unavailable("bwrap not installed".into()));
        }
        let argv = Self::argv(req);
        let t0 = Instant::now();
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut so = child.stdout.take().unwrap();
        let mut se = child.stderr.take().unwrap();
        // bounded capture: keep OUTPUT_CAP bytes, drain (and drop) the rest so a
        // flooding child cannot exhaust host memory or block on a full pipe
        async fn capped<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
            let mut keep = Vec::new();
            let _ = (&mut r)
                .take(OUTPUT_CAP as u64)
                .read_to_end(&mut keep)
                .await;
            let _ = tokio::io::copy(&mut r, &mut tokio::io::sink()).await;
            keep
        }
        let rd = async move { tokio::join!(capped(&mut so), capped(&mut se)) };
        let rd = tokio::spawn(rd);
        let (outcome, code) =
            match tokio::time::timeout(Duration::from_secs(req.limits.timeout_secs), child.wait())
                .await
            {
                Ok(st) => (Outcome::Exited, st?.code().unwrap_or(-1) as i64),
                Err(_) => {
                    let _ = child.kill().await;
                    (Outcome::TimedOut, 137)
                }
            };
        let (a, b) = rd.await.unwrap_or_default();
        Ok(SandboxResult {
            id: req.id.clone(),
            outcome,
            exit_code: code,
            stdout: String::from_utf8_lossy(&a).into(),
            stderr: String::from_utf8_lossy(&b).into(),
            duration_ms: t0.elapsed().as_millis() as u64,
            policy: serde_json::json!({"backend": "process-bwrap", "hardened": false, "network": "unshared (none)", "memory_mb": req.limits.memory_mb,
                "timeout_secs": req.limits.timeout_secs, "mounts": req.mounts, "warning": "DEV ONLY: weaker isolation than Docker"}),
        })
    }
}
