//! Real sandbox tests. They are `#[ignore]`d so a plain `cargo test` on a machine
//! without Docker/bwrap is honest about not running them, and they PANIC (never
//! skip) when run without the prerequisite. Run them with `scripts/test_docker.sh`
//! (or `cargo test -p cmsq-core -- --ignored`).
use cmsq_core::sandbox::{docker::DockerSandboxExecutor, process::ProcessSandboxExecutor, Mount, Outcome, SandboxExecutor, SandboxLimits, SandboxRequest};

async fn docker() -> DockerSandboxExecutor {
    match DockerSandboxExecutor::connect().await {
        Ok(d) => d,
        Err(e) => panic!("docker test run without a reachable docker daemon: {e} (see README: Docker setup)"),
    }
}

fn req(code: &str, ro: &std::path::Path, out: &std::path::Path, timeout: u64) -> SandboxRequest {
    std::fs::write(ro.join("probe.py"), code).unwrap();
    SandboxRequest {
        id: uuid::Uuid::new_v4().to_string(), image: "cmsq-runner:2.2.2".into(),
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
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_enforces_isolation() {
    let d = docker().await;
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d.execute(&req(PROBE, ro.path(), out.path(), 60)).await.unwrap();
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    check_probe(out.path());
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_timeout_kills() {
    let d = docker().await;
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d.execute(&req("import time\ntime.sleep(60)", ro.path(), out.path(), 3)).await.unwrap();
    assert_eq!(r.outcome, Outcome::TimedOut);
    assert!(r.duration_ms < 15_000);
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_memory_limit() {
    let d = docker().await;
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut q = req("x = bytearray(900 * 1024 * 1024)\nprint(len(x))", ro.path(), out.path(), 60);
    q.limits.memory_mb = 128;
    let r = d.execute(&q).await.unwrap();
    assert!(r.exit_code != 0, "allocation beyond limit must fail");
}

#[tokio::test]
#[ignore = "requires bubblewrap: scripts/test_docker.sh"]
async fn process_backend_isolation() {
    assert!(ProcessSandboxExecutor::available(), "bwrap not installed");
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = ProcessSandboxExecutor.execute(&req(PROBE, ro.path(), out.path(), 30)).await.unwrap();
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    check_probe(out.path());
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_identity_comes_from_image_inspect() {
    let d = docker().await;
    let id = d.runner_identity("cmsq-runner:2.2.2").await.expect("runner image missing: run scripts/build_runner.sh");
    assert!(id.starts_with("sha256:") && id.ends_with("|cmsq.runner.version=2.2.2"), "{id}");
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_output_flood_is_capped() {
    let d = docker().await;
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d.execute(&req("import sys\nsys.stdout.write('A'*(50*1024*1024))", ro.path(), out.path(), 60)).await.unwrap();
    assert_eq!(r.stdout.len(), cmsq_core::sandbox::OUTPUT_CAP);
}
