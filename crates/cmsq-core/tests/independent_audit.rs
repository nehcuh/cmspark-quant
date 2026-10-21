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
            roll_unpriced: sc.roll_unpriced,
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

mod roll_ledger {
    use cmsq_core::futures::{self, Fee, FutBar, FuturesCosts};
    use cmsq_core::scorer::CostModel;

    /// 3 days, 2 contracts; roll decided at day-2 close, effective day-3 open.
    fn fixture(old3: (f64, f64), new3: (f64, f64), new12: f64, old_vol3: f64) -> Vec<FutBar> {
        let mut all = vec![];
        for (day, (oo, oc), (no, nc), noi, ov) in [
            (2, (100.0, 100.0), (new12, new12), 100.0, 1e6),
            (3, (100.0, 100.0), (new12, new12), 300.0, 1e6),
            (4, old3, new3, 300.0, old_vol3),
        ] {
            for (c, o, cl, oi, v) in [("RB2401", oo, oc, 200.0, ov), ("RB2405", no, nc, noi, 1e6)] {
                all.push(FutBar {
                    contract: c.into(),
                    date: format!("2024-01-{day:02}"),
                    open: o,
                    high: o.max(cl),
                    low: o.min(cl),
                    close: cl,
                    settle: Some(cl),
                    volume: v,
                    oi,
                });
            }
        }
        all
    }
    fn zero_cost() -> CostModel {
        CostModel {
            commission: 0.0,
            min_commission: 0.0,
            slippage: 0.0,
            multiplier: 1.0,
        }
    }
    fn run(all: &[FutBar], pos: f64, fee: Fee) -> (cmsq_core::scorer::SimResult, f64) {
        let (bars, rolls, held) = futures::build_main_series(all, 1);
        assert_eq!(held, vec!["RB2401", "RB2401", "RB2405"]);
        let mut spec = futures::spec_for("FUT_RB").unwrap();
        spec.fee = fee;
        let sim = futures::simulate_with_rolls(
            &spec,
            &bars,
            &rolls,
            &[pos; 3],
            &FuturesCosts {
                slippage_ticks: 0.0,
                leverage: 1.0,
            },
            &zero_cost(),
        );
        (sim, spec.multiplier)
    }
    const NOFEE: Fee = Fee::PerLot {
        open: 0.0,
        close: 0.0,
        close_today: 0.0,
    };

    /// #10 reviewer case: old leg 100 -> 90 (-10%) at the roll-day open, new leg
    /// 200 -> 210. On RB a -10% open is beyond the daily limit, so a long cannot
    /// exit the old leg there: the run fails closed instead of booking +10%.
    #[test]
    fn reviewer_case_fails_closed_never_books_new_leg_gain() {
        let (sim, _) = run(
            &fixture((90.0, 90.0), (210.0, 210.0), 200.0, 1e6),
            1.0,
            NOFEE,
        );
        assert_eq!(sim.roll_unpriced, 1);
        // within the limit: the held old leg's -3% is booked, the new leg's +5% is not
        let (sim, mult) = run(
            &fixture((97.0, 97.0), (210.0, 210.0), 200.0, 1e6),
            1.0,
            NOFEE,
        );
        let lots = sim.fills.iter().find(|f| f.kind == "open").unwrap().qty;
        assert_eq!(sim.roll_unpriced, 0);
        assert_eq!(sim.equity[2], 1_000_000.0 + lots * mult * (97.0 - 100.0));
        assert_eq!(sim.equity[2], 970_000.0);
        let rc = sim.fills.iter().find(|f| f.kind == "roll_close").unwrap();
        assert_eq!(rc.price, 97.0, "old leg closes at ITS open");
    }
    /// short side, opposite gaps, no gap
    #[test]
    fn roll_short_and_gap_variants() {
        let (s, _) = run(
            &fixture((97.0, 97.0), (210.0, 210.0), 200.0, 1e6),
            -1.0,
            NOFEE,
        );
        assert_eq!(s.equity[2], 1_030_000.0, "short gains the old leg's drop");
        let (s, _) = run(
            &fixture((103.0, 103.0), (190.0, 190.0), 200.0, 1e6),
            1.0,
            NOFEE,
        );
        assert_eq!(s.equity[2], 1_030_000.0, "old up, new down");
        let (s, _) = run(
            &fixture((100.0, 100.0), (200.0, 200.0), 200.0, 1e6),
            1.0,
            NOFEE,
        );
        assert_eq!(s.equity[2], 1_000_000.0, "no gap: no P/L");
    }
    /// same price, different contract: still a roll with both legs' costs
    #[test]
    fn same_price_roll_still_pays_both_legs() {
        let fee = Fee::PerLot {
            open: 3.0,
            close: 3.0,
            close_today: 3.0,
        };
        let (s, _) = run(
            &fixture((100.0, 100.0), (100.0, 100.0), 100.0, 1e6),
            1.0,
            fee,
        );
        assert!(
            s.fills.iter().any(|f| f.kind == "roll_close")
                && s.fills.iter().any(|f| f.kind == "roll_open")
        );
    }
    /// ratio fees: the old leg's fee uses the OLD price, the new leg's the new
    #[test]
    fn ratio_fee_uses_each_legs_price() {
        let fee = Fee::Ratio {
            open: 1e-4,
            close: 1e-4,
            close_today: 1e-4,
        };
        let (s, mult) = run(&fixture((97.0, 97.0), (210.0, 210.0), 200.0, 1e6), 1.0, fee);
        let lots = s.fills.iter().find(|f| f.kind == "roll_open").unwrap().qty;
        let rc = s.fills.iter().find(|f| f.kind == "roll_close").unwrap();
        let ro = s.fills.iter().find(|f| f.kind == "roll_open").unwrap();
        assert!(
            (rc.cost - lots * 97.0 * mult * 1e-4).abs() < 1e-6,
            "{}",
            rc.cost
        );
        assert!(
            (ro.cost - lots * 210.0 * mult * 1e-4).abs() < 1e-6,
            "{}",
            ro.cost
        );
    }
    /// old leg untradeable (volume 0, limit-locked) or legacy data without legs: fail closed
    #[test]
    fn unexecutable_old_leg_fails_closed() {
        let (s, _) = run(
            &fixture((97.0, 97.0), (210.0, 210.0), 200.0, 0.0),
            1.0,
            NOFEE,
        );
        assert_eq!(s.roll_unpriced, 1, "volume 0");
        let (s, _) = run(
            &fixture((80.0, 80.0), (210.0, 210.0), 200.0, 1e6),
            1.0,
            NOFEE,
        );
        assert_eq!(
            s.roll_unpriced, 1,
            "limit-down locked: long cannot exit the old leg"
        );
        let (bars, mut rolls, _) =
            futures::build_main_series(&fixture((97.0, 97.0), (210.0, 210.0), 200.0, 1e6), 1);
        rolls[0].old_open = None;
        let spec = futures::spec_for("FUT_RB").unwrap();
        let fx = FuturesCosts::default();
        assert_eq!(
            futures::simulate_with_rolls(&spec, &bars, &rolls, &[1.0; 3], &fx, &zero_cost())
                .roll_unpriced,
            1,
            "old leg missing"
        );
        assert_eq!(
            futures::simulate(&spec, &bars, &[1.0; 3], &fx, &zero_cost()).roll_unpriced,
            1,
            "legacy series without legs"
        );
        // flat through the roll: nothing to price
        assert_eq!(
            futures::simulate(&spec, &bars, &[0.0; 3], &fx, &zero_cost()).roll_unpriced,
            0
        );
    }
    /// gate: an unpriced roll can never pass
    #[test]
    fn gate_blocks_unpriced_rolls() {
        let rolls = serde_json::json!([{"date":"2024-01-04","from":"RB2401","to":"RB2405","factor":0.5,"old_open":90.0,"old_volume":1.0}]);
        let r =
            cmsq_core::orchestrator::signed_rolls(&serde_json::json!({ "rolls": rolls })).unwrap();
        assert_eq!(r[0].old_open, Some(90.0));
        assert!(cmsq_core::orchestrator::signed_rolls(&serde_json::json!({})).is_none());
    }
}

/// #11 (OHLC finding): impossible bars are rejected at parse time.
#[test]
fn impossible_ohlc_is_rejected() {
    let bad = "date,open,high,low,close,volume\n2024-01-02,1,5,20,10,100\n";
    assert!(cmsq_core::market::parse_csv(bad).unwrap_err().to_string().contains("impossible OHLC"));
    let ok = "date,open,high,low,close,volume\n2024-01-02,10,12,9,11,100\n";
    assert_eq!(cmsq_core::market::parse_csv(ok).unwrap().len(), 1);
}
