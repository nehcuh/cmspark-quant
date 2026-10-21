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
            // per-file size cap (dev backend; never promotable — the docker
            // backend enforces the full /out byte+inode budget)
            format!("--fsize={}", req.limits.out_bytes),
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
            // Debian/Ubuntu resolve libblas etc. via /etc/alternatives symlinks
            // and the loader cache; both are non-secret, read-only.
            "--ro-bind-try".into(),
            "/etc/alternatives".into(),
            "/etc/alternatives".into(),
            "--ro-bind-try".into(),
            "/etc/ld.so.cache".into(),
            "/etc/ld.so.cache".into(),
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
        // /out is a host bind here (no size-capped tmpfs as in docker), so a watchdog
        // enforces the same byte + inode budget: poll, and kill on the first excess
        let out_dir = req
            .mounts
            .iter()
            .find(|m| m.container == "/out" && !m.read_only)
            .map(|m| m.host.clone());
        let (b_cap, i_cap) = (req.limits.out_bytes, req.limits.out_inodes);
        let over = |d: &Option<std::path::PathBuf>| {
            d.as_ref().and_then(|d| out_usage(d, b_cap, i_cap).err())
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(req.limits.timeout_secs);
        let mut quota: Option<String> = None;
        let (outcome, code) = loop {
            tokio::select! {
                st = child.wait() => {
                    let code = st?.code().unwrap_or(-1) as i64;
                    // a final check catches a burst between the last poll and exit
                    if let Some(q) = over(&out_dir) {
                        quota = Some(q);
                        break (Outcome::QuotaExceeded, code);
                    }
                    break (Outcome::Exited, code);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.kill().await;
                    break (Outcome::TimedOut, 137);
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if let Some(q) = over(&out_dir) {
                        let _ = child.kill().await;
                        quota = Some(q);
                        break (Outcome::QuotaExceeded, 137);
                    }
                }
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
                "timeout_secs": req.limits.timeout_secs, "mounts": req.mounts, "warning": "DEV ONLY: weaker isolation than Docker",
                "out": format!("host bind, watchdog-polled (50 ms) budget bytes={b_cap} files={i_cap}"), "out_quota_exceeded": quota}),
        })
    }
}

/// Walk `/out` on the host without following symlinks; `Err` names the first budget
/// exceeded (entries counted like inodes: files, dirs, symlinks, fifos alike).
pub fn out_usage(
    dir: &std::path::Path,
    max_bytes: u64,
    max_files: u64,
) -> Result<(u64, u64), String> {
    let (mut bytes, mut files) = (0u64, 0u64);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            files += 1;
            if files > max_files {
                return Err(format!("/out holds more than {max_files} entries"));
            }
            if let Ok(m) = e.path().symlink_metadata() {
                if m.is_dir() {
                    stack.push(e.path());
                } else {
                    bytes += m.len();
                    if bytes > max_bytes {
                        return Err(format!("/out holds more than {max_bytes} bytes"));
                    }
                }
            }
        }
    }
    Ok((bytes, files))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn out_usage_counts_entries_and_bytes_without_following_links() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/b"), vec![0u8; 50]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/usr", d.path().join("link")).unwrap();
        let (b, f) = out_usage(d.path(), 1_000, 10).unwrap();
        assert_eq!(f, 4);
        assert!((150..200).contains(&b), "symlink target not followed: {b}");
        assert!(out_usage(d.path(), 1_000, 3)
            .unwrap_err()
            .contains("entries"));
        assert!(out_usage(d.path(), 120, 10).unwrap_err().contains("bytes"));
    }
}
