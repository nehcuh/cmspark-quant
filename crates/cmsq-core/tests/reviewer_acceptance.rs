//! Reviewer-owned acceptance adaptation of the original 7 diagnostics. Baseline source preserved separately; only expected outcomes/current API contracts changed.
//! MockSignal never executes strategy code: it isolates trusted host logic.
use cmsq_core::extract::HeuristicExtractor;
use cmsq_core::orchestrator::{Dataset, IngestOutcome, RunOutcome};
use cmsq_core::sandbox::*;
use cmsq_core::store::{NewTrial, TrialRow};
use cmsq_core::{Home, Orchestrator, StrategyStatus};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

async fn setup() -> (tempfile::TempDir, Orchestrator, String) {
    let d = tempfile::tempdir().unwrap();
    if let Ok(path) = std::env::var("CMSQ_REVIEW_PG_ENV") {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(d.path().join("secrets")).unwrap();
        let settings = std::fs::read_to_string(path).unwrap();
        let schema = format!("review_{}", uuid::Uuid::new_v4().simple());
        let file = d.path().join("secrets/db.env");
        std::fs::write(
            &file,
            format!("{settings}\nCMSQ_DATABASE_SCHEMA={schema}\n"),
        )
        .unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let o = Orchestrator::open(Home::with_secrets(
        d.path().join("home"),
        d.path().join("secrets"),
    ))
    .unwrap();
    if std::env::var("CMSQ_REVIEW_PG_ENV").is_ok() {
        assert_eq!(o.store.backend(), "postgres");
    }
    o.ensure_samples().await.unwrap();
    let bytes = include_bytes!("../../../fixtures/posts/ma_crossover.json");
    let IngestOutcome::New(id) = o.ingest_bytes(bytes, "independent-audit").unwrap() else {
        panic!()
    };
    o.extract(&id, &HeuristicExtractor).await.unwrap();
    (d, o, id)
}

struct MockSignal {
    reopen: Option<Home>,
    did_reopen: AtomicBool,
}
#[async_trait::async_trait]
impl SandboxExecutor for MockSignal {
    fn name(&self) -> &'static str {
        "audit-test-double"
    }
    fn hardened(&self) -> bool {
        true
    }
    async fn runner_identity(&self, _: &str) -> Option<String> {
        if let Some(h) = &self.reopen {
            if !self.did_reopen.swap(true, Ordering::SeqCst) {
                let observer = Orchestrator::open(h.clone()).unwrap();
                assert_eq!(
                    observer.store.list_strategies(None).unwrap()[0].status,
                    StrategyStatus::Backtesting
                );
            }
        }
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

#[tokio::test]
async fn sample_metadata_edit_bypasses_real_data_check() {
    let (_d, o, id) = setup().await;
    assert!(!o.gate.allow_synthetic);
    let path = o.home.data_dir().join("SAMPLE_REGIME.meta.json");
    let mut m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    m["source"] = json!("tushare");
    std::fs::write(path, m.to_string()).unwrap();
    let (bars, meta, att) = o.load_dataset("SAMPLE_REGIME").await.unwrap();
    assert_eq!(att["source"], "sample");
    assert!(o.store.verify_audit().unwrap().ok);
    let s = o.get(&id).unwrap();
    let e = s.extracted.as_ref().unwrap();
    let code = e.code.as_ref().unwrap();
    let ds = Dataset {
        symbol: "SAMPLE_REGIME".into(),
        bars,
        meta,
        att,
    };
    let mock = MockSignal {
        reopen: None,
        did_reopen: AtomicBool::new(false),
    };
    let RunOutcome::Scored(sc) = o
        .evaluate_once(&s, e, code, &ds, &json!({}), &mock, "single", None)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(
        sc.synthetic,
        "signed sample provenance must remain synthetic"
    );
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
    let real = g.checks["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "real_market_data")
        .unwrap();
    println!(
        "sample signed source={}, mutable source={}, real_market_data={}",
        ds.att["source"], ds.meta.source, real
    );
    assert_eq!(real["ok"], false);
}

#[tokio::test]
async fn cross_symbol_trials_are_collapsed_in_dsr_n() {
    let (_d, o, _) = setup().await;
    for i in 0..100 {
        o.store
            .record_trial(&NewTrial {
                strategy_id: "same-strategy",
                job_id: &format!("j{i}"),
                idea_key: "same-idea",
                params_hash: "same-params",
                symbol: &format!("SH{:06}", 600000 + i),
                dataset_sha: "snapshot",
                bars: 500,
                sharpe_daily: 0.02,
                positions: &[],
                lookahead: false,
                run_kind: "single",
                group_id: None,
            })
            .unwrap();
    }
    let all: Vec<TrialRow> = o.store.trials().unwrap();
    let metrics = cmsq_core::scorer::Metrics {
        bars: 500,
        sharpe_daily: 0.17,
        kurtosis: 3.0,
        ..Default::default()
    };
    let (dsr, info) = o.dsr_for(&all, &metrics);
    let conservative = cmsq_core::scorer::deflated_sharpe_v(&metrics, dsr.sr_variance, 100);
    println!(
        "100 distinct symbols: {info}, DSR(actual N)={}, DSR(N=100)={}",
        dsr.dsr, conservative.dsr
    );
    assert_eq!(all.len(), 100);
    assert_eq!(dsr.trials, 100);
    assert!(dsr.dsr < 0.95 && (dsr.dsr - conservative.dsr).abs() < 1e-12);
}

#[tokio::test]
async fn opening_observer_reaps_active_pre_job_backtest() {
    let (_d, o, id) = setup().await;
    let mock = MockSignal {
        reopen: Some(o.home.clone()),
        did_reopen: AtomicBool::new(false),
    };
    let result = o.backtest(&id, "SAMPLE_REGIME", &mock, None).await;
    println!("active backtest after observer open: {result:?}");
    assert!(mock.did_reopen.load(Ordering::SeqCst));
    assert_eq!(o.get(&id).unwrap().status, StrategyStatus::Rejected);
    assert!(result.is_ok());
    assert_eq!(o.store.run_owner(&id).unwrap(), None);
    assert_eq!(o.store.list_jobs(Some(&id)).unwrap()[0].status, "succeeded");
}

#[tokio::test]
async fn impossible_ohlc_is_accepted_and_not_cross_checked() {
    let csv = "date,open,high,low,close,volume\n2024-01-02,1,5,20,10,100\n";
    let error = cmsq_core::market::parse_csv(csv).unwrap_err();
    println!("impossible OHLC refused: {error:#}");
    assert!(error.to_string().contains("impossible OHLC"));
}

#[test]
fn probe_has_a_deterministic_early_scoring_blind_region() {
    use cmsq_core::{market::Bar, probe};
    let code="import numpy as np\ndef generate_signals(df):\n    x=df.close.to_numpy()\n    p=np.zeros(len(x))\n    y=np.diff(x[:301])\n    p[:len(y)]=(y>0).astype(float)\n    return p\n";
    assert!(cmsq_core::extract::static_risks(Some(code), "")
        .iter()
        .all(|r| r.code != "lookahead"));
    let bars: Vec<Bar> = (0..1000)
        .map(|i| {
            let c = 100.0 + (i % 2) as f64;
            Bar {
                date: String::new(),
                open: 100.0,
                high: 102.0,
                low: 99.0,
                close: c,
                volume: 1e6,
                adj_factor: None,
            }
        })
        .collect();
    // Exact position semantics of the numpy code above: signal[t] sees close[t+1].
    let cheat = |bs: &[Bar]| -> Vec<f64> {
        let mut p = vec![0.0; bs.len()];
        for (t, w) in bs[..301.min(bs.len())].windows(2).enumerate() {
            p[t] = if w[1].close > w[0].close { 1.0 } else { 0.0 };
        }
        p
    };
    for seed in 0..50 {
        let mut rng = probe::Rng::new(seed);
        let cfg = probe::ProbeConfig::default();
        let (rc, tc) = probe::choose_cuts(bars.len(), 200, &cfg, &mut rng);
        assert!(rc.contains(&200) || tc.contains(&200));
        let cuts: Vec<usize> = rc.iter().chain(&tc).copied().collect();
        let mirrors = probe::strata_cuts(bars.len(), 200, &cfg, &mut rng);
        let plan =
            probe::plan_with_mirrors(&bars, &cuts, &rc, &mirrors, cfg.resamples, 0.095, &mut rng);
        let full = plan.iter().find(|v| v.name == "full").unwrap();
        let pos = cheat(&full.bars);
        let dates: Vec<String> = full.bars.iter().map(|b| b.date.clone()).collect();
        assert!(
            plan.iter()
                .any(|v| probe::compare(&pos, v, &cheat(&v.bars), &dates).is_some()),
            "seed {seed}"
        );
    }
    println!("early future-dependent positions at scoring bars 200..299 are detected for all 50 seeds using production mirror plan; static scanner remains clean");
}

#[tokio::test]
#[ignore = "bounded Docker check; uses existing python:3.12-slim image"]
async fn docker_output_mount_accepts_more_than_positions_cap() {
    let d = cmsq_core::sandbox::docker::DockerSandboxExecutor::connect()
        .await
        .unwrap();
    let ro = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(ro.path().join("probe.py"),"from pathlib import Path\nPath('/out/unrelated.bin').write_bytes(b'Z'*(9*1024*1024))\nprint('done')\n").unwrap();
    let q = SandboxRequest {
        id: uuid::Uuid::new_v4().to_string(),
        image: "python:3.12-slim".into(),
        command: vec!["python3".into(), "-I".into(), "/input/probe.py".into()],
        env: vec![],
        mounts: vec![
            Mount {
                host: ro.path().into(),
                container: "/input".into(),
                read_only: true,
            },
            Mount {
                host: out.path().into(),
                container: "/out".into(),
                read_only: false,
            },
        ],
        limits: SandboxLimits {
            timeout_secs: 10,
            ..Default::default()
        },
    };
    let r = d.execute(&q).await.unwrap();
    println!("Docker exit={}, stderr={}", r.exit_code, r.stderr);
    assert_ne!(r.exit_code, 0, "9 MiB output must fail");
    assert!(!out.path().join("unrelated.bin").exists());
}

#[test]
fn continuous_futures_roll_uses_unheld_contract_overnight_return() {
    use cmsq_core::futures::{self, Fee, FutBar, FuturesCosts};
    let mut all = vec![];
    for (day, old_open, old_close, new_open, new_close, new_oi) in [
        (2, 100.0, 100.0, 200.0, 200.0, 100.0),
        (3, 100.0, 100.0, 200.0, 200.0, 300.0),
        (4, 90.0, 90.0, 210.0, 210.0, 300.0),
    ] {
        for (contract, open, close, oi) in [
            ("RB2401", old_open, old_close, 200.0),
            ("RB2405", new_open, new_close, new_oi),
        ] {
            all.push(FutBar {
                contract: contract.into(),
                date: format!("2024-01-{day:02}"),
                open,
                high: open.max(close),
                low: open.min(close),
                close,
                settle: Some(close),
                volume: 1e6,
                oi,
            });
        }
    }
    let (bars, rolls, held) = futures::build_main_series(&all, 1);
    assert_eq!(rolls.len(), 1);
    assert_eq!(held, vec!["RB2401", "RB2401", "RB2405"]);
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
    let sim = futures::simulate_with_rolls(
        &spec,
        &bars,
        &rolls,
        &[1.0, 1.0, 1.0],
        &FuturesCosts {
            slippage_ticks: 0.0,
            leverage: 1.0,
        },
        &costs,
    );
    let old_lots = sim.fills.iter().find(|f| f.kind == "open").unwrap().qty;
    let actual = 1_000_000.0 + old_lots * spec.multiplier * (90.0 - 100.0);
    println!(
        "roll at next open: synthetic scorer equity={}, old-contract ledger equity={actual}",
        sim.equity[2]
    );
    assert_eq!(
        sim.roll_unpriced, 1,
        "original -10% old-leg gap must be fail-closed"
    );
    assert_eq!(actual, 900_000.0);
    for b in &mut all {
        if b.contract == "RB2401" && b.date == "2024-01-04" {
            b.open = 97.0;
            b.high = 97.0;
            b.low = 97.0;
            b.close = 97.0;
        }
        if b.contract == "RB2405" && b.date == "2024-01-04" {
            b.open = 204.0;
            b.high = 204.0;
            b.low = 204.0;
            b.close = 204.0;
        }
    }
    let (bars, rolls, _) = futures::build_main_series(&all, 1);
    let sim = futures::simulate_with_rolls(
        &spec,
        &bars,
        &rolls,
        &[1.0; 3],
        &FuturesCosts {
            slippage_ticks: 0.0,
            leverage: 1.0,
        },
        &costs,
    );
    assert_eq!(sim.roll_unpriced, 0);
    assert_eq!(sim.equity[2], 970_000.0);
    println!("executable old -3% leg ledger equity={}", sim.equity[2]);
}

#[tokio::test]
#[ignore = "Known issue #7: different date windows of equal length collapse; run explicitly to reproduce"]
async fn different_windows_with_same_bar_count_are_not_deduped() {
    let (_d, mut o, id) = setup().await;
    let (bars, meta, att) = o.load_dataset("SAMPLE_REGIME").await.unwrap();
    let ds = Dataset {
        symbol: "SAMPLE_REGIME".into(),
        bars,
        meta,
        att,
    };
    let s = o.get(&id).unwrap();
    let e = s.extracted.as_ref().unwrap();
    let code = e.code.as_ref().unwrap();
    let mock = MockSignal {
        reopen: None,
        did_reopen: AtomicBool::new(false),
    };
    let RunOutcome::Scored(a) = o
        .evaluate_once(&s, e, code, &ds, &json!({}), &mock, "single", None)
        .await
        .unwrap()
    else {
        panic!()
    };
    o.dev_frac = 0.9;
    let end = (ds.bars.len() as f64 * o.dev_frac) as usize;
    o.warmup_frac = (end - a.dm.bars) as f64 / end as f64 + 0.00001;
    let RunOutcome::Scored(b) = o
        .evaluate_once(&s, e, code, &ds, &json!({}), &mock, "single", None)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(a.dm.bars, b.dm.bars);
    assert_ne!((&a.dm.start, &a.dm.end), (&b.dm.start, &b.dm.end));
    let (_, info) = o.dsr_for(&o.store.trials().unwrap(), &b.dm);
    println!(
        "A={}..{}, B={}..{}, both bars={}, trial info={info}",
        a.dm.start, a.dm.end, b.dm.start, b.dm.end, a.dm.bars
    );
    assert_eq!(
        info["selections"], 2,
        "different date windows are different research selections"
    );
}
