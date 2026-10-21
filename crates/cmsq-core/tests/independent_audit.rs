//! Regression tests for the 2026-10-21 independent review (issues #5-#10).
//! Each started as the reviewer's diagnostic (asserting the defect on 2de65f5)
//! and now asserts the safe behaviour. MockSignal never executes strategy code.
use cmsq_core::extract::HeuristicExtractor;
use cmsq_core::orchestrator::{Dataset, IngestOutcome, RunOutcome};
use cmsq_core::sandbox::*;
use cmsq_core::{Home, Orchestrator};
use serde_json::json;

async fn setup() -> (tempfile::TempDir, Orchestrator, String) {
    setup_db(false).await
}

/// `pg`: a fresh schema on the deployed PostgreSQL (CMSQ_TEST_DB_ENV, scripts/test_pg.sh).
async fn setup_db(pg: bool) -> (tempfile::TempDir, Orchestrator, String) {
    let d = tempfile::tempdir().unwrap();
    if pg {
        let f = std::env::var("CMSQ_TEST_DB_ENV").expect("PostgreSQL required: scripts/test_pg.sh");
        let env = std::fs::read_to_string(f).unwrap();
        let schema = format!("t_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        std::fs::create_dir_all(d.path().join("secrets")).unwrap();
        std::fs::write(
            d.path().join("secrets/db.env"),
            format!("{env}\nCMSQ_DATABASE_SCHEMA={schema}\n"),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            d.path().join("secrets"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            d.path().join("secrets/db.env"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let o = Orchestrator::open(Home::with_secrets(
        d.path().join("home"),
        d.path().join("secrets"),
    ))
    .unwrap();
    o.ensure_samples().await.unwrap();
    let bytes = include_bytes!("../../../fixtures/posts/ma_crossover.json");
    let IngestOutcome::New(id) = o.ingest_bytes(bytes, "independent-audit").unwrap() else {
        panic!()
    };
    o.extract(&id, &HeuristicExtractor).await.unwrap();
    (d, o, id)
}

pub struct MockSignal;
#[async_trait::async_trait]
impl SandboxExecutor for MockSignal {
    fn name(&self) -> &'static str {
        "audit-test-double"
    }
    fn hardened(&self) -> bool {
        true
    }
    async fn runner_identity(&self, _: &str) -> Option<String> {
        Some("sha256:audit-test-double|cmsq.runner.version=2.2.2".into())
    }
    async fn execute(&self, q: &SandboxRequest) -> Result<SandboxResult, SandboxError> {
        let input = &q
            .mounts
            .iter()
            .find(|m| m.container == "/input")
            .unwrap()
            .host;
        let output = &q
            .mounts
            .iter()
            .find(|m| m.container == "/out")
            .unwrap()
            .host;
        let bars = cmsq_core::market::parse_csv(&std::fs::read_to_string(input.join("bars.csv"))?)
            .unwrap();
        let positions = cmsq_core::replay::BarPlayer::new(&bars)
            .run(&mut cmsq_core::replay::MaCrossover { fast: 10, slow: 30 });
        let mut csv = String::from("date,position\n");
        for (b, p) in bars.iter().zip(positions) {
            csv.push_str(&format!("{},{}\n", b.date, p));
        }
        std::fs::write(output.join("positions.csv"), csv)?;
        Ok(SandboxResult {
            id: q.id.clone(),
            outcome: Outcome::Exited,
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 1,
            policy: json!({"test_double":true}),
        })
    }
}

fn real_check(o: &Orchestrator, sc: &cmsq_core::orchestrator::Scored) -> serde_json::Value {
    let g = cmsq_core::gate::evaluate(
        &cmsq_core::gate::GateInput {
            job_status: "succeeded",
            hardened: sc.hardened,
            runner_identity: sc.identity.as_deref(),
            expected_runner: Some("sha256:audit-test-double"),
            dev: &sc.dm,
            dev_cost_2x: &sc.dm2,
            folds: &sc.folds,
            dsr: &sc.dsr,
            probe: &sc.probe,
            data_adjusted: sc.adjusted,
            data_validated: sc.validated.clone(),
            high_risks: &sc.high_risks,
            buy_hold: &sc.bhm,
            synthetic: sc.synthetic,
            capacity: sc.capacity,
        },
        &o.gate,
    );
    g.checks["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "real_market_data")
        .unwrap()
        .clone()
}

/// #5: editing meta.json (source/label/extra) never turns a signed sample into real data.
#[tokio::test]
async fn sample_metadata_edit_cannot_bypass_real_data_check() {
    let (_d, o, id) = setup().await;
    assert!(!o.gate.allow_synthetic);
    for sym in [
        "SAMPLE_REGIME",
        "SAMPLE_TREND",
        "SAMPLE_RANGE",
        "SAMPLE_PERSIST",
    ] {
        let path = o.home.data_dir().join(format!("{sym}.meta.json"));
        let mut m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        m["source"] = json!("tushare");
        m["label"] = json!("tushare daily (qfq)");
        m["extra"]["validated_by"] = json!("rstdx");
        std::fs::write(&path, m.to_string()).unwrap();
        let (bars, meta, att) = o.load_dataset(sym).await.unwrap();
        assert_eq!(att["source"], "sample");
        let s = o.get(&id).unwrap();
        let e = s.extracted.as_ref().unwrap();
        let ds = Dataset {
            symbol: sym.into(),
            bars,
            meta,
            att,
        };
        let RunOutcome::Scored(sc) = o
            .evaluate_once(
                &s,
                e,
                e.code.as_ref().unwrap(),
                &ds,
                &json!({}),
                &MockSignal,
                "single",
                None,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(
            sc.synthetic,
            "{sym}: signed sample provenance must decide the class"
        );
        assert_eq!(real_check(&o, &sc)["ok"], false, "{sym}");
    }
}

/// #5: only signed independent real-market sources classify as real; csv, unknown and
/// missing sources fail closed, as does any SAMPLE_ symbol.
#[test]
fn data_class_comes_only_from_signed_source() {
    use cmsq_core::orchestrator::attested_real_data as real;
    assert!(real(&json!({"source":"tushare"}), "SH600519"));
    assert!(real(&json!({"source":"rstdx"}), "SZ000001"));
    for att in [
        json!({"source":"sample"}),
        json!({"source":"csv"}),
        json!({"source":"Tushare "}),
        json!({"source":null}),
        json!({}),
    ] {
        assert!(!real(&att, "SH600519"), "{att}");
    }
    assert!(!real(&json!({"source":"tushare"}), "SAMPLE_REGIME"));
}

fn trial(o: &Orchestrator, job: &str, sym: &str, sha: &str, bars: usize, params: &str) {
    o.store
        .record_trial(&cmsq_core::store::NewTrial {
            strategy_id: "same-strategy",
            job_id: job,
            idea_key: "same-idea",
            params_hash: params,
            symbol: sym,
            dataset_sha: sha,
            bars,
            sharpe_daily: 0.02,
            positions: &[],
            lookahead: false,
            run_kind: "single",
            group_id: None,
        })
        .unwrap();
}

/// #7: choosing the best of 100 markets counts 100 selections, and the DSR verdict changes.
#[tokio::test]
async fn cross_symbol_trials_count_in_dsr_n() {
    let (_d, o, _) = setup().await;
    for i in 0..100 {
        trial(
            &o,
            &format!("j{i}"),
            &format!("SH{:06}", 600000 + i),
            "snapshot",
            500,
            "same-params",
        );
    }
    let all = o.store.trials().unwrap();
    let metrics = cmsq_core::scorer::Metrics {
        bars: 500,
        sharpe_daily: 0.17,
        kurtosis: 3.0,
        ..Default::default()
    };
    let (dsr, info) = o.dsr_for(&all, &metrics);
    let n20 = cmsq_core::scorer::deflated_sharpe_v(&metrics, dsr.sr_variance, 20);
    println!(
        "{info} DSR(N={})={} vs DSR(N=20)={}",
        dsr.trials, dsr.dsr, n20.dsr
    );
    assert_eq!(dsr.trials, 100);
    assert_eq!(info["selections"], 100);
    assert!(n20.dsr >= 0.95 && dsr.dsr < 0.95, "crossing-threshold case");
    assert!(info["counting_rule"]
        .as_str()
        .unwrap()
        .contains("no correlation discount"));
}

/// #7: exact reruns dedupe; a different window, snapshot or params is a new trial.
#[tokio::test]
async fn trial_identity_dedupes_only_exact_reruns() {
    let (_d, o, _) = setup().await;
    let n = |o: &Orchestrator| {
        o.dsr_for(&o.store.trials().unwrap(), &Default::default()).1["selections"]
            .as_u64()
            .unwrap()
    };
    trial(&o, "a", "SH600000", "snap1", 500, "p1");
    trial(&o, "b", "SH600000", "snap1", 500, "p1");
    assert_eq!(n(&o), 1, "identical rerun");
    trial(&o, "c", "SH600000", "snap1", 400, "p1");
    assert_eq!(n(&o), 2, "window");
    trial(&o, "d", "SH600000", "snap2", 500, "p1");
    assert_eq!(n(&o), 3, "snapshot");
    trial(&o, "e", "SH600000", "snap1", 500, "p2");
    assert_eq!(n(&o), 4, "params");
}

/// #6: the probe's truncation cover spans the whole scored domain. A strategy whose
/// signal[t] reads close[t+1] over a window of scored positions is caught for every
/// seed (seeds 0..50 recorded here); a causal strategy is never flagged.
#[test]
fn probe_covers_the_whole_scored_domain() {
    use cmsq_core::{market::Bar, probe};
    let code = "import numpy as np\ndef generate_signals(df):\n    x=df.close.to_numpy()\n    p=np.zeros(len(x))\n    y=np.diff(x[:301])\n    p[:len(y)]=(y>0).astype(float)\n    return p\n";
    assert!(cmsq_core::extract::static_risks(Some(code), "")
        .iter()
        .all(|r| r.code != "lookahead"));
    let n = 1000;
    let score_from = 200;
    let bars: Vec<Bar> = (0..n)
        .map(|i| {
            let c = 100.0 + ((i * 7919) % 13) as f64 * 0.3;
            Bar {
                date: String::new(),
                open: 100.0,
                high: 105.0,
                low: 99.0,
                close: c,
                volume: 1e6,
                adj_factor: None,
            }
        })
        .collect();
    // signal[t] = close[t+1] > close[t] for t in [a, b) (needs bar t+1 to exist)
    let cheat = |a: usize, b: usize| {
        move |bs: &[Bar]| -> Vec<f64> {
            let mut p = vec![0.0; bs.len()];
            for t in a..b.min(bs.len().saturating_sub(1)) {
                p[t] = if bs[t + 1].close > bs[t].close {
                    1.0
                } else {
                    0.0
                };
            }
            p
        }
    };
    let causal = |bs: &[Bar]| -> Vec<f64> {
        (0..bs.len())
            .map(|t| {
                if t >= 5 && bs[t].close > bs[t - 5..t].iter().map(|b| b.close).sum::<f64>() / 5.0 {
                    1.0
                } else {
                    0.0
                }
            })
            .collect()
    };
    let detected = |seed: u64, f: &dyn Fn(&[Bar]) -> Vec<f64>, from: usize| {
        let mut rng = probe::Rng::new(seed);
        let cfg = probe::ProbeConfig::default();
        let (rc, tc) = probe::choose_cuts(bars.len(), from, &cfg, &mut rng);
        let mc = probe::strata_cuts(bars.len(), from, &cfg, &mut rng);
        let cuts: Vec<usize> = rc.iter().chain(&tc).copied().collect();
        let plan = probe::plan_with_mirrors(&bars, &cuts, &rc, &mc, cfg.resamples, 0.095, &mut rng);
        let full = plan.iter().find(|v| v.name == "full").unwrap();
        let pos = f(&full.bars);
        let dates: Vec<String> = full.bars.iter().map(|b| b.date.clone()).collect();
        plan.iter()
            .any(|v| probe::compare(&pos, v, &f(&v.bars), &dates).is_some())
    };
    let w = probe::stratum_width(980 - score_from);
    for seed in 0..50 {
        // reviewer's case: positions 0..300 peek (early prefix incl. scoring start)
        assert!(
            detected(seed, &cheat(0, 300), score_from),
            "early prefix, seed {seed}"
        );
        // peeking only inside the scored domain, starting at the scoring start
        assert!(
            detected(seed, &cheat(score_from, score_from + 2 * w), score_from),
            "scoring start, seed {seed}"
        );
        // mid-series only, and right at a dev/vault-like boundary region
        assert!(
            detected(seed, &cheat(517, 517 + 2 * w), score_from),
            "mid, seed {seed}"
        );
        assert!(
            detected(seed, &cheat(790, 790 + 2 * w), score_from),
            "boundary, seed {seed}"
        );
        // tail only
        assert!(
            detected(seed, &cheat(n - 3, n), score_from),
            "tail, seed {seed}"
        );
        assert!(
            !detected(seed, &causal, score_from),
            "false positive, seed {seed}"
        );
    }
    // small series: duplicates removed, the scoring start is still always cut
    let mut rng = probe::Rng::new(7);
    let (_, tc) = probe::choose_cuts(60, 12, &probe::ProbeConfig::default(), &mut rng);
    assert!(tc.contains(&12), "{tc:?}");
}

/// Opens a second Orchestrator (as another CLI/dashboard process would) at every
/// runner_identity call: between Backtesting and insert_job, and between sweep children.
struct Observer(Home, std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl SandboxExecutor for Observer {
    fn name(&self) -> &'static str {
        "audit-test-double"
    }
    fn hardened(&self) -> bool {
        true
    }
    async fn runner_identity(&self, _: &str) -> Option<String> {
        let obs = Orchestrator::open(self.0.clone()).unwrap();
        for s in obs.store.list_strategies(None).unwrap() {
            assert_ne!(
                s.status,
                cmsq_core::StrategyStatus::Failed,
                "observer reaped a live run"
            );
        }
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some("sha256:audit-test-double|cmsq.runner.version=2.2.2".into())
    }
    async fn execute(&self, q: &SandboxRequest) -> Result<SandboxResult, SandboxError> {
        MockSignal.execute(q).await
    }
}

/// #8: an observer opening mid-run never fails an active backtest; it concludes normally.
#[tokio::test]
async fn observer_does_not_reap_active_backtest() {
    observer_backtest(false).await
}
#[tokio::test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
async fn pg_observer_does_not_reap_active_backtest() {
    observer_backtest(true).await
}
#[tokio::test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
async fn pg_observer_does_not_reap_active_sweep() {
    observer_sweep(true).await
}
async fn observer_backtest(pg: bool) {
    let (_d, o, id) = setup_db(pg).await;
    assert_eq!(o.store.backend(), if pg { "postgres" } else { "sqlite" });
    let obs = Observer(o.home.clone(), Default::default());
    let r = o.backtest(&id, "SAMPLE_REGIME", &obs, None).await;
    assert!(obs.1.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    let st = o.get(&id).unwrap().status;
    assert!(r.is_ok(), "{r:?}");
    assert!(
        matches!(
            st,
            cmsq_core::StrategyStatus::Evaluated
                | cmsq_core::StrategyStatus::GatePassed
                | cmsq_core::StrategyStatus::Rejected
        ),
        "{st:?}"
    );
    assert_eq!(o.store.run_owner(&id).unwrap(), None, "lease released");
}

/// #8: same between sweep children.
#[tokio::test]
async fn observer_does_not_reap_active_sweep() {
    observer_sweep(false).await
}
async fn observer_sweep(pg: bool) {
    let (_d, o, id) = setup_db(pg).await;
    let obs = Observer(o.home.clone(), Default::default());
    let grid = json!({"fast": [5, 10], "slow": [30]});
    let r = o
        .sweep(&id, "SAMPLE_REGIME", grid.as_object().unwrap(), &obs, 1)
        .await;
    assert!(r.is_ok(), "{r:?}");
    assert!(obs.1.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    assert_ne!(
        o.get(&id).unwrap().status,
        cmsq_core::StrategyStatus::Failed
    );
}

/// #8: a dead owner on this host is recovered; a lease from another host is not;
/// recovery never overwrites a finished job.
#[tokio::test]
async fn recovery_only_reaps_dead_owners() {
    let (_d, o, id) = setup().await;
    let s = o.get(&id).unwrap();
    assert!(o
        .store
        .cas_status(&id, s.status, cmsq_core::StrategyStatus::Backtesting, None)
        .unwrap());
    let mine = o.store.run_owner(&id).unwrap().unwrap();
    assert_eq!(cmsq_core::store::lease_alive(&mine), Some(true));
    // alive owner (this process): untouched
    assert!(o.reap_stale_jobs().unwrap().is_empty());
    // owner on another host: untouched (that host recovers it)
    o.store
        .set_run_owner_for_test(&id, Some("some-other-host|1|1"))
        .unwrap();
    assert!(o.reap_stale_jobs().unwrap().is_empty());
    assert_eq!(
        o.get(&id).unwrap().status,
        cmsq_core::StrategyStatus::Backtesting
    );
    // dead PID on this host
    let mut c = std::process::Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    let dead = format!("{}|{pid}|x", cmsq_core::store::hostname());
    assert_eq!(cmsq_core::store::lease_alive(&dead), Some(false));
    // PID reuse: our own PID with a different start time is dead too
    let reused = format!("{}|{}|0", cmsq_core::store::hostname(), std::process::id());
    assert_eq!(cmsq_core::store::lease_alive(&reused), Some(false));
    o.store.set_run_owner_for_test(&id, Some(&dead)).unwrap();
    assert_eq!(o.reap_stale_jobs().unwrap(), vec![id.clone()]);
    assert_eq!(
        o.get(&id).unwrap().status,
        cmsq_core::StrategyStatus::Failed
    );
    // abandon after finish is a no-op
    let (_d2, o2, id2) = setup().await;
    o2.backtest(&id2, "SAMPLE_REGIME", &MockSignal, None)
        .await
        .unwrap();
    let j = &o2.store.list_jobs(Some(&id2)).unwrap()[0];
    assert!(!o2.store.abandon_job(&j.id, "late recovery").unwrap());
    assert_eq!(o2.store.list_jobs(Some(&id2)).unwrap()[0].status, j.status);
}
