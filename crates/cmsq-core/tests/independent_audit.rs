//! Regression tests for the 2026-10-21 independent review (issues #5-#10).
//! Each started as the reviewer's diagnostic (asserting the defect on 2de65f5)
//! and now asserts the safe behaviour. MockSignal never executes strategy code.
use cmsq_core::extract::HeuristicExtractor;
use cmsq_core::orchestrator::{Dataset, IngestOutcome, RunOutcome};
use cmsq_core::sandbox::*;
use cmsq_core::{Home, Orchestrator};
use serde_json::json;

async fn setup() -> (tempfile::TempDir, Orchestrator, String) {
    let d = tempfile::tempdir().unwrap();
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
            strategy_id: "same-strategy", job_id: job, idea_key: "same-idea", params_hash: params, symbol: sym, dataset_sha: sha, bars,
            sharpe_daily: 0.02, positions: &[], lookahead: false, run_kind: "single", group_id: None,
        })
        .unwrap();
}

/// #7: choosing the best of 100 markets counts 100 selections, and the DSR verdict changes.
#[tokio::test]
async fn cross_symbol_trials_count_in_dsr_n() {
    let (_d, o, _) = setup().await;
    for i in 0..100 {
        trial(&o, &format!("j{i}"), &format!("SH{:06}", 600000 + i), "snapshot", 500, "same-params");
    }
    let all = o.store.trials().unwrap();
    let metrics = cmsq_core::scorer::Metrics { bars: 500, sharpe_daily: 0.17, kurtosis: 3.0, ..Default::default() };
    let (dsr, info) = o.dsr_for(&all, &metrics);
    let n20 = cmsq_core::scorer::deflated_sharpe_v(&metrics, dsr.sr_variance, 20);
    println!("{info} DSR(N={})={} vs DSR(N=20)={}", dsr.trials, dsr.dsr, n20.dsr);
    assert_eq!(dsr.trials, 100);
    assert_eq!(info["selections"], 100);
    assert!(n20.dsr >= 0.95 && dsr.dsr < 0.95, "crossing-threshold case");
    assert!(info["counting_rule"].as_str().unwrap().contains("no correlation discount"));
}

/// #7: exact reruns dedupe; a different window, snapshot or params is a new trial.
#[tokio::test]
async fn trial_identity_dedupes_only_exact_reruns() {
    let (_d, o, _) = setup().await;
    let n = |o: &Orchestrator| o.dsr_for(&o.store.trials().unwrap(), &Default::default()).1["selections"].as_u64().unwrap();
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
