//! Real sandbox tests. Docker tests skip (with a message) when the daemon or the
//! runner image is unavailable; bwrap tests skip when bwrap is missing.
use cmsq_core::sandbox::{docker::DockerSandboxExecutor, process::ProcessSandboxExecutor, Mount, Outcome, SandboxExecutor, SandboxLimits, SandboxRequest};

async fn docker() -> Option<DockerSandboxExecutor> {
    match DockerSandboxExecutor::connect().await {
        Ok(d) => Some(d),
        Err(e) => {
            if std::env::var("CMSQ_REQUIRE_DOCKER").is_ok() {
                panic!("CMSQ_REQUIRE_DOCKER set but docker unavailable: {e}");
            }
            eprintln!("SKIP docker tests: {e}");
            None
        }
    }
}

fn req(code: &str, ro: &std::path::Path, out: &std::path::Path, timeout: u64) -> SandboxRequest {
    std::fs::write(ro.join("probe.py"), code).unwrap();
    SandboxRequest {
        id: uuid::Uuid::new_v4().to_string(), image: "cmsq-runner:1.0.0".into(),
        command: vec!["python3".into(), "-I".into(), "/data/probe.py".into()], env: vec![],
        mounts: vec![Mount { host: ro.to_path_buf(), container: "/data".into(), read_only: true },
                     Mount { host: out.to_path_buf(), container: "/out".into(), read_only: false }],
        limits: SandboxLimits { timeout_secs: timeout, ..Default::default() },
    }
}

const PROBE: &str = r#"
import socket, os, json
res = {}
try:
    socket.create_connection(("1.1.1.1", 53), timeout=2); res["net"] = "OPEN"
except Exception as e: res["net"] = "blocked"
try:
    open("/data/x", "w"); res["data_write"] = "OPEN"
except Exception: res["data_write"] = "blocked"
try:
    open("/usr/evil", "w"); res["rootfs_write"] = "OPEN"
except Exception: res["rootfs_write"] = "blocked"
res["uid"] = os.getuid()
res["docker_sock"] = os.path.exists("/var/run/docker.sock")
open("/out/r.json", "w").write(json.dumps(res))
print(json.dumps(res))
"#;

fn check_probe(out: &std::path::Path) {
    let r: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("r.json")).unwrap()).unwrap();
    assert_eq!(r["net"], "blocked", "{r}");
    assert_eq!(r["data_write"], "blocked", "{r}");
    assert_eq!(r["rootfs_write"], "blocked", "{r}");
    assert_ne!(r["uid"], 0, "{r}");
    assert_eq!(r["docker_sock"], false, "{r}");
}

#[tokio::test]
async fn docker_enforces_isolation() {
    let Some(d) = docker().await else { return };
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d.execute(&req(PROBE, ro.path(), out.path(), 60)).await.unwrap();
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    check_probe(out.path());
}

#[tokio::test]
async fn docker_timeout_kills() {
    let Some(d) = docker().await else { return };
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d.execute(&req("import time\ntime.sleep(60)", ro.path(), out.path(), 3)).await.unwrap();
    assert_eq!(r.outcome, Outcome::TimedOut);
    assert!(r.duration_ms < 15_000);
}

#[tokio::test]
async fn docker_memory_limit() {
    let Some(d) = docker().await else { return };
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut q = req("x = bytearray(900 * 1024 * 1024)\nprint(len(x))", ro.path(), out.path(), 60);
    q.limits.memory_mb = 128;
    let r = d.execute(&q).await.unwrap();
    assert!(r.exit_code != 0, "allocation beyond limit must fail");
}

#[tokio::test]
async fn process_backend_isolation() {
    if !ProcessSandboxExecutor::available() {
        eprintln!("SKIP bwrap");
        return;
    }
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = ProcessSandboxExecutor.execute(&req(PROBE, ro.path(), out.path(), 30)).await.unwrap();
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    check_probe(out.path());
}
