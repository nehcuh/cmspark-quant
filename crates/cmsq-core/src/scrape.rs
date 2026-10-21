//! Scheduled community-strategy scraping (spec core): host-side, read-only HTTP
//! adapters for public sources turn posts/code into `cmspark.scrape.v1` drops,
//! which go through the normal ingest -> extract -> sandboxed backtest -> gate
//! pipeline. Nothing scraped is executed on the host; code only ever runs in
//! the sandbox. Sources are scored by what their items achieved downstream and
//! the per-cycle quota follows the score (with an exploration floor).
//!
//! The logged-in-browser path (CMspark) keeps writing drops into `<home>/drop`;
//! these adapters cover sources with a clean public API.
use crate::ingest::{DropAttachment, DropContent, DropScraper, DropSource, ScrapeDrop, SPEC};
use crate::models::{Strategy, StrategyStatus};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Largest single code file taken from a source.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig {
    /// GitHub repository search (public API), newest pushes first; takes the
    /// vn.py CTA strategy files (`class X(CtaTemplate)` with `on_bar`) of each hit.
    GithubSearch {
        id: String,
        query: String,
        #[serde(default = "d_repos")]
        max_repos: usize,
        #[serde(default = "d_files")]
        max_files: usize,
    },
    /// One curated repository, files under `path_prefix`.
    GithubRepo {
        id: String,
        repo: String,
        #[serde(default)]
        path_prefix: String,
        #[serde(default = "d_files")]
        max_files: usize,
    },
}
fn d_repos() -> usize {
    6
}
fn d_files() -> usize {
    8
}

impl SourceConfig {
    pub fn id(&self) -> &str {
        match self {
            Self::GithubSearch { id, .. } | Self::GithubRepo { id, .. } => id,
        }
    }
    pub fn max_files(&self) -> usize {
        match self {
            Self::GithubSearch { max_files, .. } | Self::GithubRepo { max_files, .. } => *max_files,
        }
    }
}

/// Default sources: one curated (official vn.py CTA strategies) and two
/// open-ended community searches.
pub fn default_sources() -> Vec<SourceConfig> {
    vec![
        SourceConfig::GithubRepo {
            id: "github:vnpy/vnpy_ctastrategy".into(),
            repo: "vnpy/vnpy_ctastrategy".into(),
            path_prefix: "vnpy_ctastrategy/strategies/".into(),
            max_files: 8,
        },
        SourceConfig::GithubSearch {
            id: "github-search:vnpy cta".into(),
            query: "vnpy cta strategy".into(),
            max_repos: 6,
            max_files: 8,
        },
        SourceConfig::GithubSearch {
            id: "github-search:CtaTemplate".into(),
            query: "CtaTemplate in:readme,description language:python".into(),
            max_repos: 6,
            max_files: 8,
        },
    ]
}

/// HTTP GET seam (tests use a fixture fetcher; production uses reqwest).
#[async_trait::async_trait]
pub trait Fetcher: Send + Sync {
    /// (status, body); bodies are truncated at `limit` bytes.
    async fn get(&self, url: &str, limit: usize) -> Result<(u16, String)>;
}

pub struct HttpFetcher {
    client: reqwest::Client,
    token: Option<String>,
}
impl HttpFetcher {
    /// `token`: optional GitHub token (raises the API rate limit); never logged.
    pub fn new(token: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("cmsq-scraper/0.2 (+research; read-only)")
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()?;
        Ok(Self { client, token })
    }
}
#[async_trait::async_trait]
impl Fetcher for HttpFetcher {
    async fn get(&self, url: &str, limit: usize) -> Result<(u16, String)> {
        anyhow::ensure!(
            url.starts_with("https://api.github.com/")
                || url.starts_with("https://raw.githubusercontent.com/"),
            "scraper only talks to allowlisted hosts"
        );
        let mut q = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if let (Some(t), true) = (&self.token, url.starts_with("https://api.github.com/")) {
            q = q.bearer_auth(t);
        }
        let mut r = q.send().await?;
        // redirects must stay on the allowlist too
        anyhow::ensure!(
            matches!(
                r.url().host_str(),
                Some("api.github.com" | "raw.githubusercontent.com")
            ) && r.url().scheme() == "https",
            "redirected off the allowlist to {}",
            r.url().host_str().unwrap_or("?")
        );
        let status = r.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = r.chunk().await? {
            body.extend_from_slice(&chunk);
            if body.len() > limit {
                body.truncate(limit);
                break;
            }
        }
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }
}

/// One scraped item before it becomes a drop.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub repo: String,
    pub path: String,
    pub url: String,
    pub author: String,
    pub description: String,
    pub license: String,
    pub stars: u64,
    pub pushed_at: String,
    pub code: String,
}

/// Is this file a vn.py CTA strategy worth sending to the pipeline?
pub fn is_cta_strategy(code: &str) -> bool {
    code.len() <= MAX_FILE_BYTES
        && code.contains("CtaTemplate")
        && code.contains("def on_bar")
        && regex::Regex::new(r"class\s+\w+\s*\(\s*CtaTemplate\s*\)")
            .unwrap()
            .is_match(code)
}

async fn json(f: &dyn Fetcher, url: &str) -> Result<serde_json::Value> {
    let (st, body) = f.get(url, 4 * 1024 * 1024).await?;
    anyhow::ensure!(st == 200, "GET {url}: HTTP {st}");
    serde_json::from_str(&body).with_context(|| format!("GET {url}: not JSON"))
}

/// Files of one repo matching the CTA filter (at most `max`).
async fn repo_items(
    f: &dyn Fetcher,
    meta: &serde_json::Value,
    prefix: &str,
    max: usize,
) -> Result<Vec<Item>> {
    let full = meta["full_name"].as_str().context("full_name")?.to_string();
    let branch = meta["default_branch"]
        .as_str()
        .unwrap_or("main")
        .to_string();
    let tree = json(
        f,
        &format!("https://api.github.com/repos/{full}/git/trees/{branch}?recursive=1"),
    )
    .await?;
    let mut paths: Vec<String> = tree["tree"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| {
            e["type"] == "blob" && e["size"].as_u64().unwrap_or(u64::MAX) <= MAX_FILE_BYTES as u64
        })
        .filter_map(|e| e["path"].as_str().map(String::from))
        .filter(|p| {
            p.ends_with(".py")
                && p.starts_with(prefix)
                && !p.contains("/test")
                && !p.starts_with("test")
                && !p.ends_with("__init__.py")
        })
        .collect();
    // strategy-looking names first
    paths.sort_by_key(|p| (!p.to_lowercase().contains("strateg"), p.clone()));
    let mut out = vec![];
    for p in paths.into_iter().take(max * 4) {
        if out.len() >= max {
            break;
        }
        let raw = format!("https://raw.githubusercontent.com/{full}/{branch}/{p}");
        let Ok((200, code)) = f.get(&raw, MAX_FILE_BYTES + 1).await else {
            continue;
        };
        if !is_cta_strategy(&code) {
            continue;
        }
        out.push(Item {
            repo: full.clone(),
            url: format!("https://github.com/{full}/blob/{branch}/{p}"),
            path: p,
            author: meta["owner"]["login"].as_str().unwrap_or("").into(),
            description: meta["description"].as_str().unwrap_or("").into(),
            license: meta["license"]["spdx_id"]
                .as_str()
                .unwrap_or("NOASSERTION")
                .into(),
            stars: meta["stargazers_count"].as_u64().unwrap_or(0),
            pushed_at: meta["pushed_at"].as_str().unwrap_or("").into(),
            code,
        });
    }
    Ok(out)
}

/// Fetch up to `quota` items from a source.
pub async fn fetch_source(f: &dyn Fetcher, src: &SourceConfig, quota: usize) -> Result<Vec<Item>> {
    let mut out = vec![];
    match src {
        SourceConfig::GithubRepo {
            repo, path_prefix, ..
        } => {
            let meta = json(f, &format!("https://api.github.com/repos/{repo}")).await?;
            out = repo_items(f, &meta, path_prefix, quota).await?;
        }
        SourceConfig::GithubSearch {
            query, max_repos, ..
        } => {
            let q: String = url_escape(query);
            let res = json(f, &format!("https://api.github.com/search/repositories?q={q}&sort=updated&order=desc&per_page={max_repos}")).await?;
            for meta in res["items"]
                .as_array()
                .into_iter()
                .flatten()
                .take(*max_repos)
            {
                if out.len() >= quota || meta["fork"] == true || meta["archived"] == true {
                    continue;
                }
                match repo_items(f, meta, "", quota - out.len()).await {
                    Ok(v) => out.extend(v),
                    Err(e)
                        if e.to_string().contains("HTTP 403")
                            || e.to_string().contains("HTTP 429") =>
                    {
                        return Err(e.context("rate limited"))
                    }
                    Err(_) => continue,
                }
            }
        }
    }
    Ok(out)
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Turn an item into a drop. `site` is the source id (source scoring key).
pub fn to_drop(src: &SourceConfig, it: &Item, now: &str) -> ScrapeDrop {
    let class = regex::Regex::new(r"class\s+(\w+)\s*\(\s*CtaTemplate")
        .unwrap()
        .captures(&it.code)
        .map(|c| c[1].to_string())
        .unwrap_or_default();
    ScrapeDrop {
        spec: SPEC.into(),
        sample: false,
        source: DropSource { site: src.id().into(), url: Some(it.url.clone()), author: Some(it.author.clone()), community: Some("github".into()) },
        scraped_at: now.into(),
        scraper: DropScraper { agent: "cmsq-scrape".into(), version: Some(env!("CARGO_PKG_VERSION").into()), task_id: None },
        content: DropContent {
            title: format!("{class} — {}/{}", it.repo, it.path).chars().take(200).collect(),
            format: "markdown".into(),
            body: format!(
                "vn.py CTA strategy `{class}` from {}.\n\n{}\n\nrepo stars: {}  last push: {}  license: {}",
                it.repo, it.description, it.stars, it.pushed_at, it.license
            ),
        },
        attachments: vec![DropAttachment { name: it.path.rsplit('/').next().unwrap_or("strategy.py").into(), language: Some("python".into()), content: it.code.clone() }],
        labels: vec!["vnpy".into(), "cta".into(), format!("license:{}", it.license)],
    }
}

/// Downstream quality of one source, from the strategies it produced.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct SourceScore {
    pub source: String,
    pub items: usize,
    pub extracted: usize,
    /// items with at least one sandbox job
    pub attempted: usize,
    /// items with a successful (scored) sandbox job
    pub backtested: usize,
    pub gate_passed: usize,
    pub failed: usize,
    /// smoothed score in [0, 1]
    pub score: f64,
}

/// Stage-wise smoothed rates, each judged only on items that reached the stage
/// (so a source is not penalised for items not yet backtested):
/// extract = (extracted+1)/(items+2), run = (backtested+1)/(attempted+2),
/// pass = (gate_passed+0.5)/(backtested+1); score = 0.2·extract + 0.3·run + 0.5·pass.
/// A source with no evidence scores 0.5.
pub fn score_sources(
    strategies: &[Strategy],
    attempted: &dyn Fn(&str) -> bool,
    backtested: &dyn Fn(&str) -> bool,
) -> Vec<SourceScore> {
    use std::collections::BTreeMap;
    let mut m: BTreeMap<String, SourceScore> = BTreeMap::new();
    for s in strategies.iter().filter(|s| !s.sample) {
        let e = m
            .entry(s.source_site.clone())
            .or_insert_with(|| SourceScore {
                source: s.source_site.clone(),
                ..Default::default()
            });
        e.items += 1;
        if s.extracted.as_ref().is_some_and(|x| x.code.is_some()) {
            e.extracted += 1;
        }
        if attempted(&s.id) {
            e.attempted += 1;
        }
        if backtested(&s.id) {
            e.backtested += 1;
        }
        if matches!(
            s.status,
            StrategyStatus::GatePassed | StrategyStatus::Promoted
        ) {
            e.gate_passed += 1;
        }
        if s.status == StrategyStatus::Failed {
            e.failed += 1;
        }
    }
    m.into_values()
        .map(|mut e| {
            let ext = (e.extracted as f64 + 1.0) / (e.items as f64 + 2.0);
            let run = (e.backtested as f64 + 1.0) / (e.attempted as f64 + 2.0);
            let pass = (e.gate_passed as f64 + 0.5) / (e.backtested as f64 + 1.0);
            e.score = 0.2 * ext + 0.3 * run + 0.5 * pass;
            e
        })
        .collect()
}

/// Per-cycle item quota: `base` scaled by the score, never below a 25% exploration floor.
pub fn quota(base: usize, score: Option<f64>) -> usize {
    let s = score.unwrap_or(0.5).clamp(0.0, 1.0);
    ((base as f64) * (0.25 + 1.5 * s))
        .round()
        .clamp(1.0, base as f64 * 2.0) as usize
}

/// Load `<home>/sources.json` or the defaults.
pub fn load_sources(path: &std::path::Path) -> Result<Vec<SourceConfig>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(serde_json::from_str(&t)
            .with_context(|| format!("{} is not a valid source list", path.display()))?),
        Err(_) => Ok(default_sources()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    pub struct Fixture(pub HashMap<String, (u16, String)>);
    #[async_trait::async_trait]
    impl Fetcher for Fixture {
        async fn get(&self, url: &str, limit: usize) -> Result<(u16, String)> {
            let (s, b) = self.0.get(url).cloned().unwrap_or((404, String::new()));
            Ok((s, b.chars().take(limit).collect()))
        }
    }
    const CTA: &str = "from vnpy_ctastrategy import CtaTemplate\nclass DoubleMa(CtaTemplate):\n    def on_bar(self, bar):\n        pass\n";

    fn fixture() -> Fixture {
        let mut m = HashMap::new();
        let meta = r#"{"full_name":"a/b","default_branch":"main","owner":{"login":"a"},"description":"d","license":{"spdx_id":"MIT"},"stargazers_count":3,"pushed_at":"2026-10-01T00:00:00Z","fork":false,"archived":false}"#;
        m.insert(
            "https://api.github.com/repos/a/b".into(),
            (200, meta.into()),
        );
        m.insert("https://api.github.com/search/repositories?q=vnpy+cta&sort=updated&order=desc&per_page=6".into(), (200, format!(r#"{{"items":[{meta}]}}"#)));
        m.insert(
            "https://api.github.com/repos/a/b/git/trees/main?recursive=1".into(),
            (
                200,
                r#"{"tree":[
            {"type":"blob","path":"s/double_ma_strategy.py","size":100},
            {"type":"blob","path":"s/util.py","size":100},
            {"type":"blob","path":"s/huge_strategy.py","size":999999},
            {"type":"blob","path":"tests/test_x.py","size":10}]}"#
                    .into(),
            ),
        );
        m.insert(
            "https://raw.githubusercontent.com/a/b/main/s/double_ma_strategy.py".into(),
            (200, CTA.into()),
        );
        m.insert(
            "https://raw.githubusercontent.com/a/b/main/s/util.py".into(),
            (200, "def f(): pass".into()),
        );
        Fixture(m)
    }

    #[tokio::test]
    async fn search_and_repo_sources_yield_only_cta_strategies() {
        let f = fixture();
        let s = SourceConfig::GithubSearch {
            id: "q".into(),
            query: "vnpy cta".into(),
            max_repos: 6,
            max_files: 8,
        };
        let v = fetch_source(&f, &s, 8).await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(
            v[0].url,
            "https://github.com/a/b/blob/main/s/double_ma_strategy.py"
        );
        let r = SourceConfig::GithubRepo {
            id: "r".into(),
            repo: "a/b".into(),
            path_prefix: "s/".into(),
            max_files: 8,
        };
        let d = to_drop(
            &r,
            &fetch_source(&f, &r, 8).await.unwrap()[0],
            "2026-10-21T00:00:00Z",
        );
        let bytes = serde_json::to_vec(&d).unwrap();
        let back = ScrapeDrop::parse(&bytes).unwrap();
        assert_eq!(back.source.site, "r");
        assert!(back.content.title.starts_with("DoubleMa"));
        assert!(back.labels.contains(&"license:MIT".to_string()));
        assert!(!back.sample);
    }
    #[test]
    fn scores_follow_downstream_evidence() {
        let mk = |id: &str, site: &str, st: StrategyStatus| {
            let mut s: Strategy = serde_json::from_value(serde_json::json!({"id": id, "title": "t", "source_site": site, "source_url": null, "author": null,
                "scraped_at": null, "ingested_at": "2026-01-01T00:00:00Z", "raw_path": "/x", "content_sha256": id, "sample": false, "status": "Ingested",
                "attempts": 0, "last_reason": null, "extracted": null, "updated_at": "2026-01-01T00:00:00Z"})).unwrap();
            s.status = st;
            s
        };
        let v = vec![
            mk("a", "good", StrategyStatus::GatePassed),
            mk("b", "bad", StrategyStatus::Failed),
            mk("c", "bad", StrategyStatus::Rejected),
            mk("d", "new", StrategyStatus::Ingested),
        ];
        let sc = score_sources(&v, &|id| id != "d", &|id| id == "a" || id == "c");
        let get = |s: &str| sc.iter().find(|x| x.source == s).unwrap().score;
        assert!(
            get("good") > get("new") && get("new") > get("bad"),
            "{sc:?}"
        );
        // untouched new items do not drag a source below its prior
        assert!((get("new") - (0.2 * 1.0 / 3.0 + 0.3 * 0.5 + 0.5 * 0.5)).abs() < 1e-12);
    }
    #[test]
    fn filter_and_quota() {
        assert!(is_cta_strategy(CTA));
        assert!(!is_cta_strategy("class X(CtaTemplate): pass"));
        assert!(!is_cta_strategy(&format!(
            "{CTA}{}",
            "#".repeat(MAX_FILE_BYTES)
        )));
        assert_eq!(quota(8, Some(0.0)), 2, "exploration floor");
        assert_eq!(quota(8, None), 8);
        assert_eq!(quota(8, Some(1.0)), 14);
    }
    #[tokio::test]
    async fn http_fetcher_refuses_other_hosts() {
        let f = HttpFetcher::new(None).unwrap();
        assert!(f.get("https://evil.example/x", 10).await.is_err());
        assert!(f.get("http://api.github.com/x", 10).await.is_err());
    }
}
