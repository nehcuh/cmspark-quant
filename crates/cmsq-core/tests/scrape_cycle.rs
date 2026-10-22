//! Scheduled scrape cycle end to end with a fixture fetcher (no network):
//! sources -> drops -> ingest (dedupe) -> extract -> sandbox (test double) -> scores.
use cmsq_core::sandbox::*;
use cmsq_core::scrape::{Fetcher, SourceConfig};
use cmsq_core::{Home, Orchestrator};
use std::collections::HashMap;

struct Fixture(HashMap<String, (u16, String)>);
#[async_trait::async_trait]
impl Fetcher for Fixture {
    async fn get(&self, url: &str, _: usize) -> anyhow::Result<(u16, String)> {
        Ok(self.0.get(url).cloned().unwrap_or((404, String::new())))
    }
}
struct Mock;
#[async_trait::async_trait]
impl SandboxExecutor for Mock {
    fn name(&self) -> &'static str {
        "audit-test-double"
    }
    fn hardened(&self) -> bool {
        true
    }
    async fn runner_identity(&self, _: &str) -> Option<String> {
        Some("sha256:t|cmsq.runner.version=2.2.3".into())
    }
    async fn execute(&self, q: &SandboxRequest) -> Result<SandboxResult, SandboxError> {
        let input = &q
            .mounts
            .iter()
            .find(|m| m.container == "/input")
            .unwrap()
            .host;
        let out = &q
            .mounts
            .iter()
            .find(|m| m.container == "/out")
            .unwrap()
            .host;
        let bars = cmsq_core::market::parse_csv(&std::fs::read_to_string(input.join("bars.csv"))?)
            .unwrap();
        let pos = cmsq_core::replay::BarPlayer::new(&bars)
            .run(&mut cmsq_core::replay::MaCrossover { fast: 10, slow: 30 });
        let mut csv = String::from("date,position\n");
        for (b, p) in bars.iter().zip(pos) {
            csv.push_str(&format!("{},{}\n", b.date, p));
        }
        std::fs::write(out.join("positions.csv"), csv)?;
        Ok(SandboxResult {
            id: q.id.clone(),
            outcome: Outcome::Exited,
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 1,
            policy: serde_json::json!({}),
        })
    }
}

const CODE: &str = include_str!("../../../fixtures/vnpy_examples/double_ma_strategy.py");

#[tokio::test]
async fn scrape_cycle_end_to_end_and_dedupes_on_rerun() {
    let d = tempfile::tempdir().unwrap();
    let o = Orchestrator::open(Home::with_secrets(d.path().join("h"), d.path().join("s"))).unwrap();
    o.ensure_samples().await.unwrap();
    let meta = r#"{"full_name":"a/b","default_branch":"main","owner":{"login":"a"},"description":"d","license":{"spdx_id":"MIT"},"stargazers_count":3,"pushed_at":"2026-10-01T00:00:00Z"}"#;
    let mut m = HashMap::new();
    m.insert(
        "https://api.github.com/repos/a/b".to_string(),
        (200, meta.to_string()),
    );
    m.insert(
        "https://api.github.com/repos/a/b/git/trees/main?recursive=1".into(),
        (
            200,
            r#"{"tree":[{"type":"blob","path":"strategies/double_ma_strategy.py","size":3000}]}"#
                .into(),
        ),
    );
    m.insert(
        "https://raw.githubusercontent.com/a/b/main/strategies/double_ma_strategy.py".into(),
        (200, CODE.into()),
    );
    let f = Fixture(m);
    let sources = vec![
        SourceConfig::GithubRepo {
            id: "good".into(),
            repo: "a/b".into(),
            path_prefix: "strategies/".into(),
            max_files: 4,
        },
        SourceConfig::GithubRepo {
            id: "down".into(),
            repo: "x/y".into(),
            path_prefix: String::new(),
            max_files: 4,
        },
    ];
    let rep = o
        .scrape_cycle(&f, &sources, Some(&Mock), "SAMPLE_REGIME", 2)
        .await
        .unwrap();
    assert_eq!(rep.new.len(), 1);
    assert_eq!(rep.extracted.len(), 1, "{rep:?}");
    assert_eq!(rep.backtests.len(), 1);
    assert!(
        rep.sources[1]["error"].as_str().unwrap().contains("404"),
        "a failing source is recorded, not fatal"
    );
    let s = o.get(&rep.new[0]).unwrap();
    assert_eq!(s.source_site, "good");
    assert!(!s.sample);
    let sc = o.source_scores().unwrap();
    assert_eq!(sc.len(), 1);
    assert_eq!((sc[0].items, sc[0].extracted, sc[0].backtested), (1, 1, 1));
    // the cycle is recorded as a research run and audited
    assert!(o
        .store
        .list_runs(Some("scrape"))
        .unwrap()
        .iter()
        .any(|r| r.id == rep.run_id && r.status == "done"));
    // rerun: same content -> duplicate, nothing new to backtest
    let rep2 = o
        .scrape_cycle(&f, &sources, Some(&Mock), "SAMPLE_REGIME", 2)
        .await
        .unwrap();
    assert!(rep2.new.is_empty());
    assert_eq!(rep2.duplicates, 1);
    assert!(o.store.verify_audit().unwrap().ok);
}

#[tokio::test]
async fn backlog_left_by_the_cap_is_backtested_next_cycle() {
    let d = tempfile::tempdir().unwrap();
    let o = Orchestrator::open(Home::with_secrets(d.path().join("h"), d.path().join("s"))).unwrap();
    o.ensure_samples().await.unwrap();
    let meta =
        r#"{"full_name":"a/b","default_branch":"main","owner":{"login":"a"},"license":null}"#;
    let mut m = HashMap::new();
    m.insert(
        "https://api.github.com/repos/a/b".to_string(),
        (200, meta.to_string()),
    );
    m.insert(
        "https://api.github.com/repos/a/b/git/trees/main?recursive=1".into(),
        (
            200,
            r#"{"tree":[{"type":"blob","path":"s/x.py","size":3000}]}"#.into(),
        ),
    );
    m.insert(
        "https://raw.githubusercontent.com/a/b/main/s/x.py".into(),
        (200, CODE.into()),
    );
    let src = vec![SourceConfig::GithubRepo {
        id: "g".into(),
        repo: "a/b".into(),
        path_prefix: "s/".into(),
        max_files: 2,
    }];
    let r1 = o
        .scrape_cycle(&Fixture(m.clone()), &src, Some(&Mock), "SAMPLE_REGIME", 0)
        .await
        .unwrap();
    assert_eq!((r1.extracted.len(), r1.backtests.len()), (1, 0));
    let r2 = o
        .scrape_cycle(&Fixture(m), &src, Some(&Mock), "SAMPLE_REGIME", 1)
        .await
        .unwrap();
    assert!(r2.new.is_empty());
    assert_eq!(r2.backtests.len(), 1, "backlog item picked up");
    assert_eq!(r2.backtests[0]["strategy"], r1.extracted[0].as_str());
}
