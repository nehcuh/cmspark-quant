//! Independent boundary acceptance; expected safety behavior, not author tests.
use cmsq_core::futures::{self, Fee, FutBar, FuturesCosts};
use cmsq_core::sandbox::*;
use cmsq_core::{Home, Orchestrator};

#[test]
fn legacy_sqlite_trials_migrate_without_inventing_or_merging_windows() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("old.db");
    let old = rusqlite::Connection::open(&path).unwrap();
    old.execute_batch(cmsq_core::store::SQLITE_SCHEMA).unwrap();
    for job in ["old-a", "old-b"] {
        old.execute("INSERT INTO trials(strategy_id,job_id,idea_key,params_hash,symbol,dataset_sha,bars,sharpe_daily,positions_json,created_at) VALUES('s',?1,'idea','params','S','sha',500,0.02,'[]','2024-01-01')", [job]).unwrap();
    }
    drop(old);
    let migrated =
        cmsq_core::store::Store::open(&path, cmsq_core::audit::AuditConfig::insecure_test())
            .unwrap();
    let trials = migrated.trials().unwrap();
    assert_eq!(trials.len(), 2);
    assert!(trials
        .iter()
        .all(|r| r.score_start.is_empty() && r.score_end.is_empty()));
    assert_ne!(
        trials[0].selection_key(),
        trials[1].selection_key(),
        "unknown legacy windows count conservatively, never dedupe"
    );
    assert!(
        migrated
            .raw_exec("UPDATE trials SET score_start='2020-01-01' WHERE id=1", &[])
            .is_err(),
        "migration must keep trials append-only"
    );
    drop(migrated);
    let reopened =
        cmsq_core::store::Store::open(&path, cmsq_core::audit::AuditConfig::insecure_test())
            .unwrap();
    assert_eq!(
        reopened.trials().unwrap().len(),
        2,
        "migration is idempotent"
    );
}

#[test]
#[ignore = "requires disposable PostgreSQL: CMSQ_TEST_DB_ENV"]
fn pg_legacy_window_migration_preserves_append_only_rows() {
    use cmsq_core::{audit::AuditConfig, store::Store};
    let settings = std::fs::read_to_string(std::env::var("CMSQ_TEST_DB_ENV").unwrap()).unwrap();
    let value = |key: &str| {
        settings
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .unwrap()
            .to_string()
    };
    let app = value("CMSQ_DATABASE_URL");
    let owner = value("CMSQ_DATABASE_MIGRATE_URL");
    let schema = format!("review_upgrade_{}", uuid::Uuid::new_v4().simple());
    let cfg = AuditConfig::insecure_test();
    let original = Store::open_pg(&owner, Some(&owner), Some(&schema), 4, cfg.clone()).unwrap();
    // Simulate the prior schema in this test-owned namespace only. Production
    // migration never drops columns or rewrites the append-only ledger.
    original.raw_batch("ALTER TABLE trials DROP COLUMN score_start; ALTER TABLE trials DROP COLUMN score_end; DELETE FROM _sqlx_migrations WHERE version=4; INSERT INTO trials(strategy_id,job_id,idea_key,params_hash,symbol,dataset_sha,bars,sharpe_daily,positions_json,created_at) VALUES('s','a','i','p','S','sha',500,0.02,'[]','2024-01-01'),('s','b','i','p','S','sha',500,0.02,'[]','2024-01-01');").unwrap();
    drop(original);
    let upgraded = Store::open_pg(&app, Some(&owner), Some(&schema), 4, cfg).unwrap();
    let rows = upgraded.trials().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r.score_start.is_empty() && r.score_end.is_empty()));
    assert_ne!(rows[0].selection_key(), rows[1].selection_key());
    assert!(upgraded
        .raw_exec("UPDATE trials SET score_start='2020-01-01'", &[])
        .is_err());
    println!("PostgreSQL v3 -> v4 window migration preserves two legacy rows and append-only runtime privileges");
}

#[tokio::test]
async fn signed_source_change_without_hmac_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let o = Orchestrator::open(Home::with_secrets(
        d.path().join("home"),
        d.path().join("secrets"),
    ))
    .unwrap();
    o.ensure_samples().await.unwrap();
    o.store.raw_batch("DROP TRIGGER audit_no_update;").unwrap();
    o.store.raw_exec("UPDATE audit SET details=replace(details,'\"source\":\"sample\"','\"source\":\"tushare\"') WHERE target='SAMPLE_REGIME' AND action='market_data_fetch'",&[]).unwrap();
    let err = o.load_dataset("SAMPLE_REGIME").await.unwrap_err();
    println!("mutated signed source refused: {err:#}");
    assert!(format!("{err:#}").contains("HMAC"));
}

fn roll_fixture(new_open: f64, new_volume: f64) -> Vec<FutBar> {
    let mut all = vec![];
    for (day, oo, no, noi, nv) in [
        (2, 100.0, 200.0, 100.0, 1e6),
        (3, 100.0, 200.0, 300.0, 1e6),
        (4, 97.0, new_open, 300.0, new_volume),
    ] {
        for (contract, open, oi, volume) in [("RB2401", oo, 200.0, 1e6), ("RB2405", no, noi, nv)] {
            all.push(FutBar {
                contract: contract.into(),
                date: format!("2024-01-{day:02}"),
                open,
                high: open,
                low: open,
                close: open,
                settle: Some(open),
                oi,
                volume,
            });
        }
    }
    all
}
#[test]
fn new_roll_leg_must_be_executable() {
    let mut spec = futures::spec_for("FUT_RB").unwrap();
    spec.fee = Fee::PerLot {
        open: 0.0,
        close: 0.0,
        close_today: 0.0,
    };
    let costs = cmsq_core::scorer::CostModel {
        commission: 0.0,
        min_commission: 0.0,
        slippage: 0.0,
        multiplier: 1.0,
    };
    for (direction, open, volume) in [
        (1.0, 204.0, 0.0),
        (1.0, 210.0, 1e6),
        (-1.0, 196.0, 0.0),
        (-1.0, 190.0, 1e6),
    ] {
        let (bars, rolls, _) = futures::build_main_series(&roll_fixture(open, volume), 1);
        let sim = futures::simulate_with_rolls(
            &spec,
            &bars,
            &rolls,
            &[direction; 3],
            &FuturesCosts {
                slippage_ticks: 0.0,
                leverage: 1.0,
            },
            &costs,
        );
        let fake: Vec<_> = sim.fills.iter().filter(|f| f.kind == "roll_open").collect();
        println!(
            "new leg open={open}, volume={volume}, roll_unpriced={}, roll_open={fake:?}",
            sim.roll_unpriced
        );
        assert!(
            sim.roll_unpriced > 0 || fake.is_empty(),
            "non-executable new leg must not silently qualify"
        );
    }
}

#[test]
fn unpriced_roll_is_rejected_by_actual_gate() {
    use cmsq_core::{
        gate,
        probe::ProbeReport,
        scorer::{Dsr, Metrics},
    };
    let m = Metrics {
        bars: 700,
        total_return: 0.3,
        sharpe: 1.2,
        sharpe_daily: 1.2 / 252f64.sqrt(),
        kurtosis: 3.0,
        max_drawdown: -0.1,
        exposure: 0.5,
        ..Default::default()
    };
    let bh = Metrics {
        sharpe: 0.3,
        ..m.clone()
    };
    let dsr = Dsr {
        trials: 100,
        sr_variance: 0.01,
        sr0_daily: 0.01,
        sr_daily: m.sharpe_daily,
        dsr: 0.99,
    };
    let probe = ProbeReport {
        variants: 10,
        containers: 10,
        ..Default::default()
    };
    let folds = vec![m.clone(); 4];
    let regimes = vec![
        cmsq_core::scorer::RegimeSlice {
            regime: "trend_up".into(),
            bars: 200,
            total_return: 0.1,
            market_return: 0.1,
        },
        cmsq_core::scorer::RegimeSlice {
            regime: "range".into(),
            bars: 200,
            total_return: 0.05,
            market_return: 0.0,
        },
        cmsq_core::scorer::RegimeSlice {
            regime: "vol_high".into(),
            bars: 200,
            total_return: 0.05,
            market_return: 0.0,
        },
        cmsq_core::scorer::RegimeSlice {
            regime: "vol_low".into(),
            bars: 200,
            total_return: 0.05,
            market_return: 0.0,
        },
    ];
    let mut i = gate::GateInput {
        job_status: "succeeded",
        hardened: true,
        runner_identity: Some("sha256:test|cmsq.runner.version=2.2.3"),
        expected_runner: Some("sha256:test"),
        dev: &m,
        dev_cost_2x: &m,
        folds: &folds,
        dsr: &dsr,
        probe: &probe,
        data_adjusted: true,
        data_validated: (true, "test".into()),
        high_risks: &[],
        buy_hold: &bh,
        synthetic: false,
        capacity: Some(5e7),
        roll_unpriced: 0,
        regimes: &regimes,
        model_use: &Default::default(),
        kronos_baseline: None,
    };
    assert!(gate::evaluate(&i, &gate::GateConfig::default()).passed);
    i.roll_unpriced = 1;
    let result = gate::evaluate(&i, &gate::GateConfig::default());
    println!(
        "actual development gate with unpriced roll: passed={}, reasons={:?}",
        result.passed, result.reasons
    );
    assert!(
        !result.passed
            && result
                .reasons
                .iter()
                .any(|r| r.starts_with("futures_roll_ledger"))
    );
}

fn req(code: &str, out: &std::path::Path) -> SandboxRequest {
    SandboxRequest {
        id: uuid::Uuid::new_v4().to_string(),
        image: "python:3.12-slim".into(),
        command: vec!["python3".into(), "-I".into(), "-c".into(), code.into()],
        env: vec![],
        mounts: vec![Mount {
            host: out.into(),
            container: "/out".into(),
            read_only: false,
        }],
        limits: SandboxLimits {
            timeout_secs: 10,
            log_bytes: 65536,
            ..Default::default()
        },
    }
}
fn exists(name: &str) -> bool {
    std::process::Command::new("docker")
        .args(["inspect", name])
        .output()
        .unwrap()
        .status
        .success()
}
fn cleanup(name: &str) {
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", name])
        .output();
}

#[tokio::test]
#[ignore = "bounded Docker acceptance"]
async fn docker_inode_and_live_log_limits_hold() {
    let d = cmsq_core::sandbox::docker::DockerSandboxExecutor::connect()
        .await
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let q=req("from pathlib import Path\nn=0\ntry:\n for i in range(64):\n  Path('/out/f'+str(i)).write_text('x'); n+=1\nexcept OSError: pass\nprint('FILES',n)\n",out.path());
    let r = d.execute(&q).await.unwrap();
    println!("inode probe: {}", r.stdout);
    let count: usize = r.stdout.split_whitespace().last().unwrap().parse().unwrap();
    assert!(count < 16);
    assert!(!exists(&format!("cmsq-{}", q.id)));
    let q = req("import os\nwhile True: os.write(2,b'x'*8192)\n", out.path());
    let r = d.execute(&q).await.unwrap();
    println!(
        "live budget outcome={:?}, duration={}ms",
        r.outcome, r.duration_ms
    );
    assert_eq!(r.outcome, Outcome::QuotaExceeded);
    assert!(r.duration_ms < 10000);
    assert!(!exists(&format!("cmsq-{}", q.id)));
}

#[tokio::test]
#[ignore = "bounded Docker cleanup acceptance"]
async fn docker_host_copy_error_cleans_container() {
    let d = cmsq_core::sandbox::docker::DockerSandboxExecutor::connect()
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("not-a-directory");
    std::fs::write(&out, "x").unwrap();
    let q = req(
        "from pathlib import Path\nPath('/out/positions.csv').write_text('date,position\\n')\n",
        &out,
    );
    let name = format!("cmsq-{}", q.id);
    let result = d.execute(&q).await;
    let leaked = exists(&name);
    cleanup(&name);
    println!(
        "host-copy result={result:?}; container remained={leaked}; reviewer cleanup completed"
    );
    assert!(result.is_err());
    assert!(!leaked, "host I/O error must remove job container");
}

#[tokio::test]
#[ignore = "bounded Docker cancellation acceptance"]
async fn docker_cancellation_cleans_container() {
    let d = cmsq_core::sandbox::docker::DockerSandboxExecutor::connect()
        .await
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let q = req("import time; time.sleep(10)", out.path());
    let name = format!("cmsq-{}", q.id);
    let h = tokio::spawn(async move { d.execute(&q).await });
    for _ in 0..20 {
        if exists(&name) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(exists(&name));
    h.abort();
    assert!(h.await.unwrap_err().is_cancelled());
    for _ in 0..20 {
        if !exists(&name) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let leaked = exists(&name);
    cleanup(&name);
    println!("cancelled container remained={leaked}; reviewer cleanup completed");
    assert!(
        !leaked,
        "dropping cancelled execution must remove container"
    );
}

/// Issue #19: the daemon commits `create` but the response is held; the caller is
/// cancelled inside that window. A unix-socket proxy forwards to the real daemon and
/// withholds only the create response until the test releases it (deterministic barrier).
#[tokio::test]
#[ignore = "bounded Docker create-window cancellation acceptance"]
async fn cancellation_before_create_response_must_not_leave_container() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Notify;

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("proxy.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let created = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let (created, release) = (created.clone(), release.clone());
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let (created, release) = (created.clone(), release.clone());
                tokio::spawn(async move {
                    let up = UnixStream::connect("/var/run/docker.sock").await.unwrap();
                    let (mut cr, mut cw) = client.into_split();
                    let (mut ur, mut uw) = up.into_split();
                    let is_create = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let flag = is_create.clone();
                    tokio::spawn(async move {
                        let mut b = vec![0u8; 65536];
                        while let Ok(n) = cr.read(&mut b).await {
                            if n == 0 {
                                break;
                            }
                            if String::from_utf8_lossy(&b[..n]).contains("/containers/create") {
                                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                            if uw.write_all(&b[..n]).await.is_err() {
                                break;
                            }
                        }
                    });
                    let mut b = vec![0u8; 65536];
                    while let Ok(n) = ur.read(&mut b).await {
                        if n == 0 {
                            break;
                        }
                        if is_create.swap(false, std::sync::atomic::Ordering::SeqCst) {
                            created.notify_one(); // daemon answered: the container exists
                            release.notified().await; // hold the response
                        }
                        if cw.write_all(&b[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }
    let d = cmsq_core::sandbox::docker::DockerSandboxExecutor::connect_unix(&sock)
        .await
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let mut q = req("print('must never run')", out.path());
    q.id = format!("acceptance-createwin-{}", uuid::Uuid::new_v4().simple());
    let name = format!("cmsq-{}", q.id);
    let h = tokio::spawn(async move { d.execute(&q).await });
    tokio::time::timeout(std::time::Duration::from_secs(20), created.notified())
        .await
        .expect("create never reached the daemon");
    assert!(exists(&name), "daemon should have created the container");
    h.abort();
    assert!(h.await.unwrap_err().is_cancelled());
    release.notify_one();
    let mut leaked = true;
    for _ in 0..50 {
        if !exists(&name) {
            leaked = false;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    cleanup(&name);
    assert!(
        !leaked,
        "container created before the response must be removed after cancel"
    );
}

/// #19 follow-up: the create result was delivered into the oneshot (send succeeded) but
/// the owner future is dropped before polling the receiver again.
#[tokio::test]
#[ignore = "bounded Docker create-handoff cancellation acceptance"]
async fn cancel_after_create_sent_but_before_receiver_is_polled() {
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Notify;
    struct Notice(AtomicBool);
    impl Wake for Notice {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("proxy.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let created = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    {
        let (created, release) = (created.clone(), release.clone());
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let (created, release) = (created.clone(), release.clone());
                tokio::spawn(async move {
                    let up = UnixStream::connect("/var/run/docker.sock").await.unwrap();
                    let (mut cr, mut cw) = client.into_split();
                    let (mut ur, mut uw) = up.into_split();
                    let is_create = Arc::new(AtomicBool::new(false));
                    let flag = is_create.clone();
                    tokio::spawn(async move {
                        let mut b = vec![0u8; 65536];
                        while let Ok(n) = cr.read(&mut b).await {
                            if n == 0 {
                                break;
                            }
                            if String::from_utf8_lossy(&b[..n]).contains("/containers/create") {
                                flag.store(true, Ordering::SeqCst);
                            }
                            if uw.write_all(&b[..n]).await.is_err() {
                                break;
                            }
                        }
                    });
                    let mut b = vec![0u8; 65536];
                    while let Ok(n) = ur.read(&mut b).await {
                        if n == 0 {
                            break;
                        }
                        if is_create.swap(false, Ordering::SeqCst) {
                            created.notify_one();
                            release.notified().await;
                        }
                        if cw.write_all(&b[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }
    let d = cmsq_core::sandbox::docker::DockerSandboxExecutor::connect_unix(&sock)
        .await
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let mut q = req("print('must never run')", out.path());
    q.id = format!("acceptance-handoff-{}", uuid::Uuid::new_v4().simple());
    let name = format!("cmsq-{}", q.id);
    let notice = Arc::new(Notice(AtomicBool::new(false)));
    let waker = Waker::from(notice.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = Box::pin(d.execute(&q));
    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
    tokio::time::timeout(std::time::Duration::from_secs(20), created.notified())
        .await
        .expect("create never reached the daemon");
    release.notify_one();
    for _ in 0..300 {
        if notice.0.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(notice.0.load(Ordering::SeqCst), "receiver never woken");
    assert!(exists(&name));
    drop(future); // never poll the receiver again
    let mut leaked = true;
    for _ in 0..50 {
        if !exists(&name) {
            leaked = false;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    cleanup(&name);
    assert!(
        !leaked,
        "delivered-but-untaken create result must still clean up"
    );
}
