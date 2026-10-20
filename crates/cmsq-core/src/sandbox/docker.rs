use super::*;
use bollard::container::{Config, CreateContainerOptions, KillContainerOptions, LogOutput, LogsOptions, RemoveContainerOptions, WaitContainerOptions};
use bollard::models::HostConfig;
use bollard::Docker;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Docker backend via bollard. Hardening applied to every container:
/// NetworkMode=none + network_disabled, read-only rootfs, CapDrop=ALL,
/// no-new-privileges, non-root user (the uid owning the job's /out dir, never 0;
/// falls back to 10001), pids/memory(no swap)/cpu limits,
/// tmpfs /tmp (noexec), only `/out` writable, never the docker socket, hard timeout
/// (kill + force remove).
/// Run as the (unprivileged, non-root) owner of the writable `/out` mount so no
/// world-writable directories are needed; never root.
pub fn run_as_user(req: &SandboxRequest) -> String {
    use std::os::unix::fs::MetadataExt;
    req.mounts.iter().find(|m| !m.read_only).and_then(|m| std::fs::metadata(&m.host).ok())
        .map(|md| (md.uid(), md.gid())).filter(|(u, _)| *u != 0)
        .map(|(u, g)| format!("{u}:{g}")).unwrap_or_else(|| "10001:10001".into())
}

pub struct DockerSandboxExecutor {
    docker: Docker,
}

impl DockerSandboxExecutor {
    pub async fn connect() -> Result<Self, SandboxError> {
        let docker = Docker::connect_with_local_defaults().map_err(|e| SandboxError::Unavailable(e.to_string()))?;
        docker.ping().await.map_err(|e| SandboxError::Unavailable(format!("docker ping failed: {e}")))?;
        Ok(Self { docker })
    }

    pub fn host_config(req: &SandboxRequest) -> HostConfig {
        let mem = (req.limits.memory_mb * 1024 * 1024) as i64;
        HostConfig {
            network_mode: Some("none".into()),
            readonly_rootfs: Some(true),
            privileged: Some(false),
            cap_drop: Some(vec!["ALL".into()]),
            security_opt: Some(vec!["no-new-privileges:true".into()]),
            pids_limit: Some(req.limits.pids),
            memory: Some(mem),
            memory_swap: Some(mem),
            nano_cpus: Some((req.limits.cpu_cores * 1e9) as i64),
            binds: Some(req.mounts.iter().map(|m| format!("{}:{}:{}", m.host.display(), m.container, if m.read_only { "ro" } else { "rw" })).collect()),
            tmpfs: Some(HashMap::from([("/tmp".to_string(), "rw,noexec,nosuid,size=64m".to_string())])),
            ipc_mode: Some("private".into()),
            // hide host clocks / boot time (btime) / uptime from the strategy
            masked_paths: Some(["/proc/stat", "/proc/uptime", "/proc/loadavg", "/proc/timer_list", "/proc/sched_debug", "/proc/kcore", "/proc/keys",
                "/proc/latency_stats", "/proc/acpi", "/proc/scsi", "/sys/firmware", "/proc/interrupts", "/proc/driver/rtc"].map(String::from).to_vec()),
            ..Default::default()
        }
    }
}

#[async_trait]
impl SandboxExecutor for DockerSandboxExecutor {
    fn name(&self) -> &'static str {
        "docker"
    }
    fn hardened(&self) -> bool {
        true
    }
    async fn runner_identity(&self, image: &str) -> Option<String> {
        let i = self.docker.inspect_image(image).await.ok()?;
        let ver = i.config.and_then(|c| c.labels).and_then(|l| l.get("cmsq.runner.version").cloned()).unwrap_or_else(|| "none".into());
        Some(format!("{}|cmsq.runner.version={ver}", i.id.unwrap_or_default()))
    }
    async fn execute(&self, req: &SandboxRequest) -> Result<SandboxResult, SandboxError> {
        validate(req)?;
        let host = Self::host_config(req);
        let policy = serde_json::json!({
            "backend": "docker", "network_mode": "none", "readonly_rootfs": true, "cap_drop": ["ALL"],
            "no_new_privileges": true, "user": run_as_user(req), "memory_mb": req.limits.memory_mb, "swap": false,
            "cpu_cores": req.limits.cpu_cores, "pids": req.limits.pids, "timeout_secs": req.limits.timeout_secs,
            "mounts": req.mounts, "docker_socket_mounted": false,
        });
        let name = format!("cmsq-{}", req.id);
        let cfg = Config {
            image: Some(req.image.clone()),
            entrypoint: Some(req.command.clone()),
            cmd: Some(vec![]),
            env: Some(req.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            user: Some(run_as_user(req)),
            working_dir: Some("/tmp".into()),
            network_disabled: Some(true),
            host_config: Some(host),
            labels: Some(HashMap::from([("cmsq.job".to_string(), req.id.clone())])),
            ..Default::default()
        };
        let created = self.docker.create_container(Some(CreateContainerOptions { name: name.clone(), platform: None }), cfg)
            .await.map_err(|e| SandboxError::Execution(format!("create: {e}")))?;
        let id = created.id;
        let t0 = Instant::now();
        let run = async {
            self.docker.start_container::<String>(&id, None).await.map_err(|e| SandboxError::Execution(format!("start: {e}")))?;
            let mut w = self.docker.wait_container(&id, None::<WaitContainerOptions<String>>);
            let mut code = -1;
            while let Some(r) = w.next().await {
                match r {
                    Ok(x) => code = x.status_code,
                    Err(bollard::errors::Error::DockerContainerWaitError { code: c, .. }) => code = c,
                    Err(e) => return Err(SandboxError::Execution(format!("wait: {e}"))),
                }
            }
            Ok::<i64, SandboxError>(code)
        };
        let (outcome, exit_code) = match tokio::time::timeout(Duration::from_secs(req.limits.timeout_secs), run).await {
            Ok(Ok(code)) => {
                let oom = self.docker.inspect_container(&id, None).await.ok()
                    .and_then(|i| i.state).and_then(|s| s.oom_killed).unwrap_or(false);
                (if oom { Outcome::OomKilled } else { Outcome::Exited }, code)
            }
            Ok(Err(e)) => {
                let _ = self.docker.remove_container(&id, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
                return Err(e);
            }
            Err(_) => {
                let _ = self.docker.kill_container(&id, Some(KillContainerOptions { signal: "SIGKILL" })).await;
                (Outcome::TimedOut, 137)
            }
        };
        let duration_ms = t0.elapsed().as_millis() as u64;
        let (mut stdout, mut stderr) = (String::new(), String::new());
        const CAP: usize = OUTPUT_CAP;
        let mut logs = self.docker.logs(&id, Some(LogsOptions::<String> { stdout: true, stderr: true, ..Default::default() }));
        while let Some(Ok(l)) = logs.next().await {
            match l {
                LogOutput::StdOut { message } if stdout.len() < CAP => stdout.push_str(&String::from_utf8_lossy(&message[..message.len().min(CAP - stdout.len())])),
                LogOutput::StdErr { message } if stderr.len() < CAP => stderr.push_str(&String::from_utf8_lossy(&message[..message.len().min(CAP - stderr.len())])),
                _ => {}
            }
        }
        let _ = self.docker.remove_container(&id, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
        Ok(SandboxResult { id: req.id.clone(), outcome, exit_code, stdout, stderr, duration_ms, policy })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_config_is_hardened() {
        let req = SandboxRequest { id: "t".into(), image: "x".into(), command: vec![], env: vec![],
            mounts: vec![Mount { host: "/tmp/d".into(), container: "/data".into(), read_only: true }], limits: SandboxLimits::default() };
        let h = DockerSandboxExecutor::host_config(&req);
        assert_eq!(h.network_mode.as_deref(), Some("none"));
        assert_eq!(h.readonly_rootfs, Some(true));
        assert_eq!(h.privileged, Some(false));
        assert_eq!(h.cap_drop, Some(vec!["ALL".to_string()]));
        assert_eq!(h.memory, h.memory_swap);
        assert_eq!(h.binds.unwrap(), vec!["/tmp/d:/data:ro".to_string()]);
    }
}
