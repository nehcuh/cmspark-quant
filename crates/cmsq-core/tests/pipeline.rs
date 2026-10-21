//! Full pipeline through the Orchestrator (same path as CLI and dashboard).
//! Docker/bwrap tests are `#[ignore]`d and panic when their prerequisite is
//! missing; run them with scripts/test_docker.sh.
use cmsq_core::error::CoreError;
use cmsq_core::extract::HeuristicExtractor;
use cmsq_core::orchestrator::IngestOutcome;
use cmsq_core::sandbox::{docker::DockerSandboxExecutor, process::ProcessSandboxExecutor};
use cmsq_core::{Home, Orchestrator, StrategyStatus};

fn read(dir: &str, n: &str) -> Vec<u8> {
    std::fs::read(format!("{}/../../fixtures/{dir}/{n}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

async fn setup() -> (tempfile::TempDir, Orchestrator) {
    let d = tempfile::tempdir().unwrap();
    let o = Orchestrator::open(Home::with_secrets(d.path().join("home"), d.path().join("secrets"))).unwrap();
    o.ensure_samples().await.unwrap();
    (d, o)
}

async fn add(o: &Orchestrator, dir: &str, n: &str) -> Option<String> {
    match o.ingest_bytes(&read(dir, n), n).unwrap() {
        IngestOutcome::New(id) => {
            o.extract(&id, &HeuristicExtractor).await.unwrap();
            Some(id)
        }
        _ => None,
    }
}

async fn docker() -> DockerSandboxExecutor {
    DockerSandboxExecutor::connect().await.unwrap_or_else(|e| panic!("docker required for this test: {e} (see README: Docker setup)"))
}

#[tokio::test]
async fn ingest_dedupes_rejects_invalid_and_symlinks() {
    let (d, o) = setup().await;
    assert!(matches!(o.ingest_bytes(&read("posts", "ma_crossover"), "a").unwrap(), IngestOutcome::New(_)));
    assert!(matches!(o.ingest_bytes(&read("posts", "ma_crossover"), "b").unwrap(), IngestOutcome::Duplicate(_)));
    assert!(matches!(o.ingest_bytes(b"{}", "c").unwrap(), IngestOutcome::Invalid(_)));
    assert!(matches!(o.ingest_bytes(&read("attacks", "evil_deep"), "deep").unwrap(), IngestOutcome::Invalid(_)));
    // symlink in the drop folder is not followed
    let secret = d.path().join("secret.json");
    std::fs::write(&secret, read("posts", "momentum_20d")).unwrap();
    std::os::unix::fs::symlink(&secret, o.home.drop_dir().join("link.json")).unwrap();
    let r = o.ingest_path(&o.home.drop_dir()).unwrap();
    assert_eq!(r.len(), 1);
    assert!(matches!(&r[0].1, IngestOutcome::Invalid(m) if m.contains("symlink")), "{:?}", r[0].1);
    let id = o.store.list_strategies(None).unwrap()[0].id.clone();
    // cannot backtest before extraction; typed error
    let e = o.backtest(&id, "SAMPLE_TREND", &ProcessSandboxExecutor, None).await.unwrap_err();
    assert!(matches!(e.downcast_ref::<CoreError>(), Some(CoreError::InvalidTransition { .. })), "{e:#}");
    assert!(o.store.verify_audit().unwrap().ok);
}

#[tokio::test]
async fn crash_recovery_reaps_stuck_backtests() {
    let d = tempfile::tempdir().unwrap();
    let home = Home::with_secrets(d.path().join("home"), d.path().join("secrets"));
    let sid = {
        let o = Orchestrator::open(home.clone()).unwrap();
        o.ensure_samples().await.unwrap();
        let id = add(&o, "posts", "ma_crossover").await.unwrap();
        // simulate a crash mid-backtest: status Backtesting + a running job owned by a dead pid
        o.store.set_status(&id, StrategyStatus::Backtesting, None).unwrap();
        o.store.raw_exec("INSERT INTO jobs(id,strategy_id,backend,image,symbol,data_source,status,started_at,job_dir,owner_pid,kind) VALUES('j1',?1,'docker','x','S','s','running','2026-01-01T00:00:00Z','/tmp',2147483646,'dev')", cmsq_core::p![id.as_str()]).unwrap();
        id
    };
    let o = Orchestrator::open(home).unwrap();
    let s = o.store.get_strategy(&sid).unwrap();
    assert_eq!(s.status, StrategyStatus::Failed);
    assert!(s.last_reason.unwrap().contains("interrupted"));
    assert_eq!(o.store.list_jobs(Some(&sid)).unwrap()[0].status, "abandoned");
}

#[tokio::test]
async fn audit_key_and_anchor_live_outside_home() {
    let (d, o) = setup().await;
    assert!(d.path().join("secrets/audit.key").exists());
    assert!(!o.home.anchor_file().starts_with(o.home.abs()));
    o.store.audit("t", "x", None, serde_json::json!({}), true).unwrap();
    let r = o.store.verify_audit().unwrap();
    assert!(r.ok && r.keyed && r.anchored_seq == Some(r.entries), "{r:?}");
    // a key inside CMSQ_HOME is refused
    let bad = Home::with_secrets(d.path().join("h2"), d.path().join("h2/secrets"));
    std::fs::create_dir_all(&bad.root).unwrap();
    assert!(Orchestrator::open(bad).is_err());
}

#[tokio::test]
#[ignore = "requires bubblewrap: scripts/test_docker.sh"]
async fn dev_backend_results_can_never_be_promoted() {
    assert!(ProcessSandboxExecutor::available(), "bwrap not installed");
    let (_d, o) = setup().await;
    let id = add(&o, "posts", "short_term_reversal").await.unwrap();
    let r = o.backtest(&id, "SAMPLE_RANGE", &ProcessSandboxExecutor, None).await.unwrap();
    assert_eq!(r.status, StrategyStatus::Rejected, "{:?}", r.reasons);
    assert!(r.reasons.iter().any(|x| x.starts_with("hardened_sandbox")), "{:?}", r.reasons);
    assert!(r.reasons.iter().any(|x| x.starts_with("runner_pinned")), "{:?}", r.reasons);
    let e = o.promote(&id, "test", &format!("PROMOTE {}", &id[..8]), &ProcessSandboxExecutor).await.unwrap_err();
    assert!(matches!(e.downcast_ref::<CoreError>(), Some(CoreError::NotGatePassed { .. })), "{e:#}");
}

#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_pipeline_gate_then_vault_promote() {
    let d = docker().await;
    let (_t, mut o) = setup().await;
    // explicit, test-only relaxation of the community-selection DSR floor (default 20)
    // so the vault path can be exercised on the synthetic sample
    o.gate.dsr_min_trials = 1;
    let id = add(&o, "posts", "ma_crossover").await.unwrap();
    let r = o.backtest(&id, "SAMPLE_REGIME", &d, None).await.unwrap();
    assert_eq!(r.status, StrategyStatus::GatePassed, "{:?}", r.reasons);
    let start = o.store.audit_entries(Some(&id), 200).unwrap().into_iter().find(|e| e.action == "sandbox_start").unwrap();
    assert!(start.details["probe"]["seed"].as_str().is_some() && start.details["probe"]["tail_cuts"].as_array().unwrap().len() >= 2, "{}", start.details["probe"]);
    // native host replay (BarPlayer) reproduces the sandboxed runner's positions
    let bars = cmsq_core::market::read_bars(&o.home.data_dir(), "SAMPLE_REGIME").unwrap();
    let t = o.store.trials().unwrap().into_iter().find(|t| t.strategy_id == id).unwrap();
    let view = cmsq_core::probe::container_view(&bars[..t.positions.len()]);
    let native = cmsq_core::replay::BarPlayer::new(&view).run(&mut cmsq_core::replay::MaCrossover { fast: 10, slow: 30 });
    let same = native.iter().zip(&t.positions).filter(|(a, b)| (*a - *b).abs() < 1e-9).count();
    assert!(same as f64 >= 0.999 * native.len() as f64, "native replay agrees on {same}/{} bars", native.len());
    let job = &o.store.list_jobs(Some(&id)).unwrap()[0];
    let res = job.result.as_ref().unwrap();
    assert_eq!(res["computed_by"], "cmsq-core scorer (host, trusted)");
    assert!(res["lookahead_probe"]["containers"].as_u64().unwrap() >= 7);
    assert!(job.runner_identity.as_deref().unwrap().contains("cmsq.runner.version=2.2.2"));
    // typed confirmation: id prefix alone is not enough
    let e = o.promote(&id, "test", &id[..8], &d).await.unwrap_err();
    assert!(matches!(e.downcast_ref::<CoreError>(), Some(CoreError::ConfirmationMismatch { .. })));
    let s = o.promote(&id, "test", &format!("PROMOTE {}", &id[..8]), &d).await.unwrap();
    assert!(matches!(s.status, StrategyStatus::Promoted | StrategyStatus::Rejected), "{:?}", s.status);
    assert!(o.store.vault_touch(&id).unwrap().is_some());
    let res = &o.store.list_jobs(Some(&id)).unwrap()[0];
    assert!(res.result.as_ref().unwrap()["trade_stats"]["display_only"] == true);
    if s.status == StrategyStatus::Promoted {
        assert_eq!(o.store.list_signals().unwrap().len(), 1);
        // optional QIFI export of the observe-only paper account
        let (p, q) = o.export_qifi(&id, None).unwrap();
        assert!(p.exists() && q["cmsq"]["observe_only"] == true && q["orders"] == serde_json::json!({}));
        assert_eq!(q["cmsq"]["signals"].as_array().unwrap().len(), 1);
        assert!(o.store.audit_entries(Some(&id), 200).unwrap().iter().any(|e| e.action == "qifi_export"));
    } else {
        assert!(o.export_qifi(&id, None).is_err(), "only Promoted strategies export");
    }
    // vault is one-shot
    assert!(o.promote(&id, "test", &format!("PROMOTE {}", &id[..8]), &d).await.is_err());
    // ... and per IDEA: a re-post of the same idea with different text gets no fresh vault
    let mut repost: serde_json::Value = serde_json::from_slice(&read("posts", "ma_crossover")).unwrap();
    repost["content"]["title"] = "双均线 v2（换了个标题再发一次）".into();
    repost["content"]["body"] = repost["content"]["body"].as_str().unwrap().replace("思路", "新思路").into();
    let IngestOutcome::New(id2) = o.ingest_bytes(&serde_json::to_vec(&repost).unwrap(), "repost").unwrap() else { panic!("repost deduped") };
    o.extract(&id2, &HeuristicExtractor).await.unwrap();
    let r2 = o.backtest(&id2, "SAMPLE_REGIME", &d, None).await.unwrap();
    assert_eq!(r2.status, StrategyStatus::GatePassed, "{:?}", r2.reasons);
    let e = o.promote(&id2, "test", &format!("PROMOTE {}", &id2[..8]), &d).await.unwrap_err();
    assert!(matches!(e.downcast_ref::<CoreError>(), Some(CoreError::VaultSpent(_))), "{e:#}");
    assert_eq!(o.store.get_strategy(&id2).unwrap().status, StrategyStatus::Rejected);
    assert!(o.store.verify_audit().unwrap().ok);
}

/// H3: data integrity is enforced from the audited snapshot, not from meta.json.
#[tokio::test]
async fn data_tampering_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, o) = setup().await;
    let dd = o.home.data_dir();
    assert!(o.load_dataset("SAMPLE_REGIME").await.is_ok());
    let integrity = |e: anyhow::Error| matches!(e.downcast_ref::<CoreError>(), Some(CoreError::DataIntegrity(_) | CoreError::NoData(_)));
    // 1. drop snapshot_sha256 from meta.json -> refused
    let mp = dd.join("SAMPLE_REGIME.meta.json");
    let orig = std::fs::read_to_string(&mp).unwrap();
    let mut m: serde_json::Value = serde_json::from_str(&orig).unwrap();
    m["extra"].as_object_mut().unwrap().remove("snapshot_sha256");
    std::fs::write(&mp, m.to_string()).unwrap();
    assert!(integrity(o.load_dataset("SAMPLE_REGIME").await.unwrap_err()));
    std::fs::write(&mp, &orig).unwrap();
    // 2. make the snapshot writable and edit a close -> refused (mode, then hash)
    let sha = m["extra"]["snapshot_sha256"].as_str().map(String::from).unwrap_or_else(|| serde_json::from_str::<serde_json::Value>(&orig).unwrap()["extra"]["snapshot_sha256"].as_str().unwrap().to_string());
    let sp = dd.join("snapshots").join(format!("{sha}.csv"));
    std::fs::set_permissions(&sp, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(integrity(o.load_dataset("SAMPLE_REGIME").await.unwrap_err()));
    let text = std::fs::read_to_string(&sp).unwrap().replacen(",1", ",2", 1);
    std::fs::write(&sp, text).unwrap();
    std::fs::set_permissions(&sp, std::fs::Permissions::from_mode(0o444)).unwrap();
    assert!(integrity(o.load_dataset("SAMPLE_REGIME").await.unwrap_err()));
    // 3. a user CSV relabelled source=sample is refused; as csv it can't pass cross-validation
    let csv = _t.path().join("x.csv");
    let bars = cmsq_core::market::read_bars(&dd, "SAMPLE_TREND").unwrap();
    std::fs::write(&csv, cmsq_core::market::to_csv(&bars, false)).unwrap();
    o.fetch_data(&cmsq_core::market::CsvSource { path: csv, adjustment: "hfq".into() }, "SH600999", usize::MAX).await.unwrap();
    let (_, _, att) = o.load_dataset("SH600999").await.unwrap();
    assert_eq!(att["validation_ok"], false);
    assert_eq!(att["adjusted"], false, "hfq label without raw prices + adj_factor is not 'adjusted'");
    let mp = dd.join("SH600999.meta.json");
    let mut m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&mp).unwrap()).unwrap();
    m["source"] = "sample".into();
    std::fs::write(&mp, m.to_string()).unwrap();
    let (_, _, att) = o.load_dataset("SH600999").await.unwrap();
    assert_eq!(att["source"], "csv", "verdicts come from the audited record, not meta.json");
    // 4. sample source cannot serve a non-sample symbol
    assert!(o.fetch_data(&cmsq_core::market::SampleSource, "SH600000", 100).await.is_err());
}

/// Every reviewer attack post must end Failed/Rejected (or never get past ingest).
#[tokio::test]
#[ignore = "requires docker: scripts/test_docker.sh"]
async fn docker_reviewer_attacks_never_pass() {
    let d = docker().await;
    let (_t, o) = setup().await;
    let attacks = ["forge", "memo_peek", "data_peek", "hidden_peek", "length_tell", "symlink_out", "escape", "forkbomb", "bigout", "evil_path", "evil_deep",
        "r2_clock_dev", "r2_clock_dev_bh", "r2_clock_vault", "r2_clock_probe", "r2_cut_aware", "r2_rng_alias", "r2_sparse_peek"];
    for a in attacks {
        let Some(id) = add(&o, "attacks", a).await else { continue };
        let s = o.store.get_strategy(&id).unwrap();
        let st = if s.status == StrategyStatus::Extracted { o.backtest(&id, "SAMPLE_TREND", &d, None).await.unwrap().status } else { s.status };
        assert!(matches!(st, StrategyStatus::Failed | StrategyStatus::Rejected), "attack {a} ended {st}");
        eprintln!("attack {a:12} -> {st}: {}", o.store.get_strategy(&id).unwrap().last_reason.unwrap_or_default().chars().take(140).collect::<String>());
    }
    // the look-ahead attacks without a static `shift(-n)` must be caught by the probe itself
    for a in ["hidden_peek", "memo_peek", "length_tell"] {
        let s = o.store.list_strategies(None).unwrap().into_iter().find(|s| s.title == format!("ADV_{a}")).unwrap();
        let ev = o.store.latest_evaluation(&s.id).unwrap().unwrap();
        assert!(ev.reasons.iter().any(|r| r.starts_with("no_lookahead")), "{a}: {:?}", ev.reasons);
    }
    // round 2: cut-aware / sparse / np.roll peeks are caught by the (randomised, tail-covering) probe
    for t in ["均线趋势 + 大波动过滤", "ADV2_sparse_peek", "ADV2_rng_alias"] {
        let s = o.store.list_strategies(None).unwrap().into_iter().find(|s| s.title == t).unwrap();
        let ev = o.store.latest_evaluation(&s.id).unwrap().unwrap();
        assert!(ev.reasons.iter().any(|r| r.starts_with("no_lookahead") || r.starts_with("static_risks")), "{t}: {:?}", ev.reasons);
    }
    // the sandbox shows neither the real clock nor real bar dates: the probe strategy never goes long
    let s = o.store.list_strategies(None).unwrap().into_iter().find(|s| s.title == "ADV2_clock_probe").unwrap();
    let job = o.store.list_jobs(Some(&s.id)).unwrap().into_iter().next().unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    assert_eq!(job.result.unwrap()["development"]["exposure"].as_f64().unwrap(), 0.0, "clock or real dates leaked into the sandbox");
    for (a, want) in [("bigout", "output_cap|oom"), ("symlink_out", "bad_output"), ("data_peek", "strategy_error"), ("forkbomb", "pids_exhausted")] {
        let s = o.store.list_strategies(None).unwrap().into_iter().find(|s| s.title == format!("ADV_{a}")).unwrap();
        let got = o.store.list_jobs(Some(&s.id)).unwrap()[0].status.clone();
        assert!(want.split('|').any(|w| w == got), "{a}: {got}");
    }
    assert!(o.store.verify_audit().unwrap().ok);
}

#[tokio::test]
async fn qifi_export_of_promoted_paper_account() {
    use cmsq_core::market::Bar;
    let (_d, o) = setup().await;
    let id = add(&o, "posts", "ma_crossover").await.unwrap();
    assert!(o.export_qifi(&id, None).is_err(), "not promoted yet");
    let spec = cmsq_core::futures::spec_for("FUT_RB").unwrap();
    let bars: Vec<Bar> = (0..10).map(|i| Bar { date: format!("2024-01-{:02}", i + 2), open: 4000.0 + i as f64, high: 4010.0, low: 3990.0, close: 4001.0 + i as f64, volume: 1e5, adj_factor: Some(1.0) }).collect();
    let pos: Vec<f64> = (0..10).map(|i| if i < 6 { 1.0 } else { -0.5 }).collect();
    let sim = cmsq_core::futures::simulate(&spec, &bars, &pos, &Default::default(), &Default::default());
    let acct = cmsq_core::qifi::account_inputs("FUT_RB", Some(&spec), &sim, &bars, 5);
    let job = cmsq_core::JobRecord { id: uuid::Uuid::new_v4().to_string(), strategy_id: id.clone(), backend: "docker".into(), image: "x".into(), symbol: "FUT_RB".into(),
        data_source: "t".into(), status: "running".into(), exit_code: None, started_at: chrono::Utc::now(), finished_at: None, duration_ms: None, job_dir: "/tmp".into(),
        error: None, result: None, owner_pid: None, runner_identity: None, kind: "vault".into(), owner_host: None };
    o.store.insert_job(&job).unwrap();
    o.store.finish_job(&job.id, "succeeded", Some(0), 1, None, Some(&serde_json::json!({"passed": true, "account": acct}))).unwrap();
    o.store.insert_signal(&cmsq_core::Signal { strategy_id: id.clone(), job_id: job.id.clone(), symbol: "FUT_RB".into(), date: "2024-01-11".into(), target_position: -0.5, created_at: chrono::Utc::now() }).unwrap();
    o.store.set_status(&id, StrategyStatus::Promoted, None).unwrap();
    let out = _d.path().join("q.json");
    let (p, q) = o.export_qifi(&id, Some(&out)).unwrap();
    assert_eq!(p, out);
    let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    assert_eq!(disk, q);
    assert!(q["positions"]["SHFE.RB"]["volume_short"].as_f64().unwrap() > 0.0, "{q:#}");
    assert!(q["trades"].as_object().unwrap().values().any(|t| t["offset"] == "CLOSE"), "flip inside the vault window closes the long");
    assert_eq!(q["orders"], serde_json::json!({}));
    assert_eq!(q["cmsq"]["signals"][0]["target_position"], -0.5);
    let a = o.store.audit_entries(Some(&id), 50).unwrap().into_iter().find(|e| e.action == "qifi_export").unwrap();
    assert_eq!(a.details["sha256"].as_str().unwrap().len(), 64);
}
