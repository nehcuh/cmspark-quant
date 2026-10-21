//! Real sandbox tests. They are `#[ignore]`d so a plain `cargo test` on a machine
//! without Docker/bwrap is honest about not running them, and they PANIC (never
//! skip) when run without the prerequisite. Run them with `scripts/test_docker.sh`
//! (or `cargo test -p cmsq-core -- --ignored`).
use cmsq_core::sandbox::{
    docker::DockerSandboxExecutor, process::ProcessSandboxExecutor, Mount, Outcome,
    SandboxExecutor, SandboxLimits, SandboxRequest,
};

async fn docker() -> DockerSandboxExecutor {
    match DockerSandboxExecutor::connect().await {
        Ok(d) => d,
        Err(e) => panic!(
            "docker test run without a reachable docker daemon: {e} (see README: Docker setup)"
        ),
    }
}

fn req(code: &str, ro: &std::path::Path, out: &std::path::Path, timeout: u64) -> SandboxRequest {
    std::fs::write(ro.join("probe.py"), code).unwrap();
    SandboxRequest {
        id: uuid::Uuid::new_v4().to_string(),
        image: "cmsq-runner:2.2.2".into(),
        command: vec!["python3".into(), "-I".into(), "/data/probe.py".into()],
        env: vec![],
        mounts: vec![
            Mount {
                host: ro.to_path_buf(),
                container: "/data".into(),
                read_only: true,
            },
            Mount {
                host: out.to_path_buf(),
                container: "/out".into(),
                read_only: false,
            },
        ],
        limits: SandboxLimits {
            timeout_secs: timeout,
            ..Default::default()
        },
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
open("/out/positions.csv", "w").write(json.dumps(res))  # the one file the host gets back
print(json.dumps(res))
"#;

fn check_probe(out: &std::path::Path) {
    let r: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("positions.csv")).unwrap()).unwrap();
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
    let r = d
        .execute(&req(PROBE, ro.path(), out.path(), 60))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    check_probe(out.path());
}

const CLOCK_PROBE: &str = r#"
import ctypes, json, os, time
libc = ctypes.CDLL(None, use_errno=True)
buf = (ctypes.c_long * 2)()
res = {"time_time_year": time.gmtime(time.time()).tm_year}
for name, nr, clk in [("realtime", 228, 0), ("realtime_coarse", 228, 5), ("tai", 228, 11), ("boottime", 228, 7), ("monotonic", 228, 1)]:
    r = libc.syscall(nr, clk, buf)
    res[name] = "ok" if r == 0 else "errno%d" % ctypes.get_errno()
r = libc.syscall(96, buf, None)   # gettimeofday
res["gettimeofday"] = "ok" if r == 0 else "errno%d" % ctypes.get_errno()
r = libc.syscall(201, None)       # time
res["time"] = "ok" if r >= 0 and r < 10**12 and r > 0 else "errno%d" % ctypes.get_errno()
res["monotonic_py"] = time.monotonic() > 0
open("/out/positions.csv", "w").write(json.dumps(res))
"#;

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_denies_raw_wall_clock_syscalls() {
    let d = docker().await;
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d
        .execute(&req(CLOCK_PROBE, ro.path(), out.path(), 60))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path().join("positions.csv")).unwrap())
            .unwrap();
    assert_eq!(v["time_time_year"], 2000, "libfaketime frozen clock: {v}");
    for k in [
        "realtime",
        "realtime_coarse",
        "tai",
        "boottime",
        "gettimeofday",
        "time",
    ] {
        assert_eq!(v[k], "errno1", "{k} must be denied: {v}");
    }
    assert_eq!(v["monotonic"], "ok", "{v}");
    assert_eq!(v["monotonic_py"], true, "{v}");
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_timeout_kills() {
    let d = docker().await;
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d
        .execute(&req(
            "import time\ntime.sleep(60)",
            ro.path(),
            out.path(),
            3,
        ))
        .await
        .unwrap();
    assert_eq!(r.outcome, Outcome::TimedOut);
    assert!(r.duration_ms < 15_000);
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_memory_limit() {
    let d = docker().await;
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut q = req(
        "x = bytearray(900 * 1024 * 1024)\nprint(len(x))",
        ro.path(),
        out.path(),
        60,
    );
    q.limits.memory_mb = 128;
    let r = d.execute(&q).await.unwrap();
    assert!(r.exit_code != 0, "allocation beyond limit must fail");
}

#[tokio::test]
#[ignore = "requires bubblewrap: scripts/test_docker.sh"]
async fn process_backend_isolation() {
    assert!(ProcessSandboxExecutor::available(), "bwrap not installed");
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = ProcessSandboxExecutor
        .execute(&req(PROBE, ro.path(), out.path(), 30))
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    check_probe(out.path());
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_identity_comes_from_image_inspect() {
    let d = docker().await;
    let id = d
        .runner_identity("cmsq-runner:2.2.2")
        .await
        .expect("runner image missing: run scripts/build_runner.sh");
    assert!(
        id.starts_with("sha256:") && id.ends_with("|cmsq.runner.version=2.2.2"),
        "{id}"
    );
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_output_flood_is_capped() {
    let d = docker().await;
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d
        .execute(&req(
            "import sys\nsys.stdout.write('A'*(50*1024*1024))",
            ro.path(),
            out.path(),
            60,
        ))
        .await
        .unwrap();
    assert_eq!(r.stdout.len(), cmsq_core::sandbox::OUTPUT_CAP);
    assert_eq!(
        r.outcome,
        Outcome::QuotaExceeded,
        "streamed output is killed live"
    );
}

/// #9: /out is a kernel-capped tmpfs; nothing but positions.csv reaches the host.
#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_out_budget_is_enforced_while_running() {
    let d = docker().await;
    // one big unrelated file (the reviewer's 9 MiB probe) with a small budget
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut q = req("from pathlib import Path\ntry:\n    Path('/out/unrelated.bin').write_bytes(b'Z'*(9*1024*1024)); print('WROTE')\nexcept OSError as e: print('ENOSPC', e.errno); Path('/out/unrelated.bin').unlink()\nopen('/out/positions.csv','w').write('ok')\n", ro.path(), out.path(), 30);
    q.limits.out_bytes = 1024 * 1024;
    let r = d.execute(&q).await.unwrap();
    assert!(r.stdout.contains("ENOSPC"), "{} {}", r.stdout, r.stderr);
    assert!(
        !out.path().join("unrelated.bin").exists(),
        "unrelated files never reach the host"
    );
    assert_eq!(
        std::fs::read_to_string(out.path().join("positions.csv")).unwrap(),
        "ok"
    );
    // many small files: inode budget
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let r = d.execute(&req("n=0\ntry:\n    for i in range(10000): open(f'/out/f{i}','w').write('x'); n+=1\nexcept OSError: pass\nprint('FILES', n)\n", ro.path(), out.path(), 30)).await.unwrap();
    let n: usize = r.stdout.trim().rsplit(' ').next().unwrap().parse().unwrap();
    assert!(n < 16, "inode budget: wrote {n} files");
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    // continuous small-chunk stderr stream: killed live, container removed
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let q = req(
        "import sys\nwhile True: sys.stderr.write('E'*4096)\n",
        ro.path(),
        out.path(),
        60,
    );
    let t0 = std::time::Instant::now();
    let r = d.execute(&q).await.unwrap();
    assert_eq!(r.outcome, Outcome::QuotaExceeded);
    assert!(
        t0.elapsed().as_secs() < 30,
        "killed by budget, not by timeout"
    );
    let gone = std::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("label=cmsq.job={}", q.id)])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&gone.stdout).trim().is_empty(),
        "container removed"
    );
}

#[tokio::test]
#[ignore = "requires bubblewrap: scripts/test_docker.sh"]
async fn process_backend_caps_out_file_count() {
    assert!(ProcessSandboxExecutor::available(), "bwrap not installed");
    let (ro, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let code = "import time\nfor i in range(200000):\n    open('/out/f%d' % i, 'w').close()\n    if i % 500 == 0: time.sleep(0.01)\n";
    let r = ProcessSandboxExecutor
        .execute(&req(code, ro.path(), out.path(), 60))
        .await
        .unwrap();
    assert_eq!(r.outcome, Outcome::QuotaExceeded, "{}", r.stderr);
    let n = std::fs::read_dir(out.path()).unwrap().count();
    assert!(n < 20_000, "watchdog stopped the flood early: {n} files");
    assert!(r.policy["out_quota_exceeded"]
        .as_str()
        .unwrap()
        .contains("entries"));
}
