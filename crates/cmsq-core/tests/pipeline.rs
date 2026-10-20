//! Full pipeline through the Orchestrator (same path as CLI and dashboard).
use cmsq_core::extract::HeuristicExtractor;
use cmsq_core::orchestrator::IngestOutcome;
use cmsq_core::sandbox::{docker::DockerSandboxExecutor, process::ProcessSandboxExecutor};
use cmsq_core::{Home, Orchestrator, StrategyStatus};

fn fixture(n: &str) -> Vec<u8> {
    std::fs::read(format!("{}/../../fixtures/posts/{n}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

async fn setup() -> (tempfile::TempDir, Orchestrator) {
    let d = tempfile::tempdir().unwrap();
    let o = Orchestrator::open(Home::new(d.path())).unwrap();
    o.ensure_samples().await.unwrap();
    (d, o)
}

async fn add(o: &Orchestrator, n: &str) -> String {
    let IngestOutcome::New(id) = o.ingest_bytes(&fixture(n), n).unwrap() else { panic!("not new") };
    o.extract(&id, &HeuristicExtractor).await.unwrap();
    id
}

#[tokio::test]
async fn dev_backend_results_can_never_be_promoted() {
    if !ProcessSandboxExecutor::available() {
        return;
    }
    let (_d, o) = setup().await;
    let id = add(&o, "short_term_reversal").await;
    let r = o.backtest(&id, "SAMPLE_RANGE", &ProcessSandboxExecutor, None).await.unwrap();
    assert_eq!(r.status, StrategyStatus::Rejected, "{:?}", r.reasons);
    assert!(r.reasons.iter().all(|x| x.starts_with("hardened_sandbox")), "only the backend check should fail: {:?}", r.reasons);
    assert!(o.promote(&id, "test", &id[..8]).is_err());
    let leak = add(&o, "future_peek").await;
    let r = o.backtest(&leak, "SAMPLE_TREND", &ProcessSandboxExecutor, None).await.unwrap();
    assert_eq!(r.status, StrategyStatus::Failed);
    assert!(o.store.verify_audit().unwrap().ok);
}

#[tokio::test]
async fn ingest_dedupes_and_rejects_invalid() {
    let (_d, o) = setup().await;
    assert!(matches!(o.ingest_bytes(&fixture("ma_crossover"), "a").unwrap(), IngestOutcome::New(_)));
    assert!(matches!(o.ingest_bytes(&fixture("ma_crossover"), "b").unwrap(), IngestOutcome::Duplicate(_)));
    assert!(matches!(o.ingest_bytes(b"{}", "c").unwrap(), IngestOutcome::Invalid(_)));
    let id = o.store.list_strategies(None).unwrap()[0].id.clone();
    // cannot backtest before extraction, and the denial is audited
    assert!(o.backtest(&id, "SAMPLE_TREND", &ProcessSandboxExecutor, None).await.is_err());
}

#[tokio::test]
async fn docker_pipeline_promotes_with_confirmation() {
    let Ok(d) = DockerSandboxExecutor::connect().await else {
        if std::env::var("CMSQ_REQUIRE_DOCKER").is_ok() { panic!("docker required") }
        eprintln!("SKIP docker pipeline");
        return;
    };
    let (_t, o) = setup().await;
    let id = add(&o, "short_term_reversal").await;
    let r = o.backtest(&id, "SAMPLE_RANGE", &d, None).await.unwrap();
    assert_eq!(r.status, StrategyStatus::Evaluated, "{:?}", r.reasons);
    assert!(o.promote(&id, "test", "wrong").is_err());
    let s = o.promote(&id, "test", &id[..8]).unwrap();
    assert_eq!(s.status, StrategyStatus::Promoted);
    assert_eq!(o.store.list_signals().unwrap().len(), 1);
    let net = add(&o, "phone_home").await;
    let r = o.backtest(&net, "SAMPLE_TREND", &d, None).await.unwrap();
    assert_eq!(r.status, StrategyStatus::Failed);
    assert!(r.reasons[0].contains("unreachable") || r.reasons[0].contains("Network"), "{:?}", r.reasons);
    assert!(o.store.verify_audit().unwrap().ok);
}
