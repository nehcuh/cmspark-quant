//! Core orchestrator: owns state transitions, audit emission and sandbox dispatch.
//! It never hands docker credentials to other components; the dashboard and CLI
//! only call these methods.
use crate::extract::Extractor;
use crate::gate::{self, GateConfig};
use crate::home::Home;
use crate::ingest::ScrapeDrop;
use crate::market::{self, MarketDataSource};
use crate::models::*;
use crate::sandbox::{Mount, Outcome, SandboxExecutor, SandboxLimits, SandboxRequest};
use crate::state;
use crate::store::Store;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::json;
use std::path::{Path, PathBuf};

pub const RUNNER_IMAGE: &str = "cmsq-runner:1.0.0";
pub const RUNNER_CMD: [&str; 4] = ["python3", "-I", "-B", "/runner/run_backtest.py"];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CostModel {
    pub commission: f64,
    pub stamp_duty_sell: f64,
    pub slippage: f64,
}
impl Default for CostModel {
    /// A-share defaults: 2.5bp commission each side, 5bp stamp duty on sells (since 2023-08), 5bp slippage.
    fn default() -> Self {
        Self { commission: 0.00025, stamp_duty_sell: 0.0005, slippage: 0.0005 }
    }
}

pub struct Orchestrator {
    pub home: Home,
    pub store: Store,
    pub gate: GateConfig,
    pub costs: CostModel,
    pub train_frac: f64,
    pub limits: SandboxLimits,
}

#[derive(Debug, serde::Serialize)]
pub enum IngestOutcome {
    New(String),
    Duplicate(String),
    Invalid(String),
}

#[derive(Debug, serde::Serialize)]
pub struct BacktestSummary {
    pub strategy_id: String,
    pub job_id: String,
    pub status: StrategyStatus,
    pub job_status: String,
    pub reasons: Vec<String>,
    pub oos: Option<serde_json::Value>,
}

impl Orchestrator {
    pub fn open(home: Home) -> Result<Self> {
        home.init()?;
        let store = Store::open(&home.db())?;
        Ok(Self { home, store, gate: GateConfig::default(), costs: CostModel::default(), train_frac: 0.7, limits: SandboxLimits::default() })
    }

    fn transition(&self, s: &Strategy, to: StrategyStatus, actor: &str, reason: Option<&str>, details: serde_json::Value) -> Result<()> {
        if let Err(e) = state::check(s.status, to) {
            self.store.audit(actor, "transition_denied", Some(&s.id), json!({"from": s.status, "to": to, "error": e.to_string()}), false)?;
            return Err(e);
        }
        self.store.set_status(&s.id, to, reason)?;
        self.store.audit(actor, &format!("transition:{}->{}", s.status, to), Some(&s.id), json!({"reason": reason, "details": details}), true)?;
        Ok(())
    }

    // ---------------- ingest ----------------
    pub fn ingest_bytes(&self, bytes: &[u8], origin: &str) -> Result<IngestOutcome> {
        let d = match ScrapeDrop::parse(bytes) {
            Ok(d) => d,
            Err(e) => {
                self.store.audit("ingest", "ingest_rejected", None, json!({"origin": origin, "error": format!("{e:#}")}), false)?;
                return Ok(IngestOutcome::Invalid(format!("{e:#}")));
            }
        };
        let sha = d.content_sha256();
        if let Some(id) = self.store.find_by_sha(&sha)? {
            self.store.audit("ingest", "ingest_duplicate", Some(&id), json!({"origin": origin}), true)?;
            return Ok(IngestOutcome::Duplicate(id));
        }
        // Internal UUID only; nothing from the drop is ever used as a path component.
        let id = uuid::Uuid::new_v4().to_string();
        let raw_dir = self.home.raw_dir(&id);
        std::fs::create_dir_all(&raw_dir)?;
        let raw = raw_dir.join("post.json");
        std::fs::write(&raw, bytes)?;
        let now = Utc::now();
        let s = Strategy {
            id: id.clone(), title: d.content.title.chars().take(200).collect(), source_site: d.source.site.clone(), source_url: d.source.url.clone(),
            author: d.source.author.clone(), scraped_at: Some(d.scraped_at.clone()), ingested_at: now, raw_path: raw.display().to_string(),
            content_sha256: sha.clone(), sample: d.sample, status: StrategyStatus::Ingested, attempts: 0, last_reason: None, extracted: None, updated_at: now,
        };
        self.store.insert_strategy(&s)?;
        self.store.audit("ingest", "ingest", Some(&id), json!({"origin": origin, "site": d.source.site, "url": d.source.url, "scraped_at": d.scraped_at,
            "scraper": d.scraper, "sha256": sha, "sample": d.sample, "labels": d.labels}), true)?;
        Ok(IngestOutcome::New(id))
    }

    /// Ingest every *.json in the drop dir (or a given file/dir). Processed files are
    /// moved to drop/processed, invalid ones to drop/rejected (with a .reason file).
    pub fn ingest_path(&self, path: &Path) -> Result<Vec<(PathBuf, IngestOutcome)>> {
        let mut files = vec![];
        if path.is_dir() {
            for e in std::fs::read_dir(path)? {
                let p = e?.path();
                if p.extension().is_some_and(|x| x == "json") && p.is_file() {
                    files.push(p);
                }
            }
        } else {
            files.push(path.to_path_buf());
        }
        files.sort();
        let in_drop = std::fs::canonicalize(path).ok().map(|p| p.starts_with(std::fs::canonicalize(self.home.drop_dir()).unwrap_or_default())).unwrap_or(false);
        let mut out = vec![];
        for f in files {
            let meta = std::fs::metadata(&f)?;
            let o = if meta.len() > crate::ingest::MAX_DROP_BYTES {
                self.store.audit("ingest", "ingest_rejected", None, json!({"origin": f.display().to_string(), "error": "too large"}), false)?;
                IngestOutcome::Invalid("file too large".into())
            } else {
                self.ingest_bytes(&std::fs::read(&f)?, &f.display().to_string())?
            };
            if in_drop {
                let name = f.file_name().unwrap();
                match &o {
                    IngestOutcome::Invalid(r) => {
                        let dst = self.home.drop_dir().join("rejected").join(name);
                        std::fs::rename(&f, &dst)?;
                        std::fs::write(dst.with_extension("reason.txt"), r)?;
                    }
                    _ => std::fs::rename(&f, self.home.drop_dir().join("processed").join(name))?,
                }
            }
            out.push((f, o));
        }
        Ok(out)
    }

    pub fn load_post(&self, s: &Strategy) -> Result<ScrapeDrop> {
        ScrapeDrop::parse(&std::fs::read(&s.raw_path)?)
    }

    // ---------------- extract ----------------
    pub async fn extract(&self, id: &str, ex: &dyn Extractor) -> Result<Strategy> {
        let s = self.store.get_strategy(id)?;
        if s.status != StrategyStatus::Ingested {
            bail!("strategy {} is {}, extract requires Ingested", s.id, s.status);
        }
        let post = self.load_post(&s)?;
        let e = match ex.extract(&post).await {
            Ok(e) => e,
            Err(err) => {
                self.transition(&s, StrategyStatus::Failed, "extractor", Some(&format!("extraction error: {err:#}")), json!({}))?;
                return self.store.get_strategy(&s.id);
            }
        };
        let dir = self.home.strategy_dir(&s.id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("extracted.json"), serde_json::to_string_pretty(&e)?)?;
        self.store.set_extracted(&s.id, &e)?;
        let high: Vec<&Risk> = e.risks.iter().filter(|r| r.severity == Severity::High && (r.code == "no_code" || r.code == "contract")).collect();
        if let Some(code) = &e.code {
            std::fs::write(dir.join("strategy.py"), code)?;
        }
        if !high.is_empty() {
            let reason = high.iter().map(|r| r.message.clone()).collect::<Vec<_>>().join("; ");
            self.transition(&s, StrategyStatus::Failed, "extractor", Some(&reason), json!({"extractor": e.extractor}))?;
        } else {
            self.transition(&s, StrategyStatus::Extracted, "extractor", None,
                json!({"extractor": e.extractor, "params": e.parameters, "risks": e.risks.iter().map(|r| &r.code).collect::<Vec<_>>()}))?;
        }
        self.store.get_strategy(&s.id)
    }

    // ---------------- market data ----------------
    pub async fn fetch_data(&self, src: &dyn MarketDataSource, symbol: &str, count: usize) -> Result<market::DataMeta> {
        match src.daily_bars(symbol, count).await {
            Ok((bars, meta)) => {
                market::write_cache(&self.home.data_dir(), &bars, &meta)?;
                self.store.audit("data", "market_data_fetch", Some(symbol), json!({"source": src.name(), "meta": meta}), true)?;
                Ok(meta)
            }
            Err(e) => {
                self.store.audit("data", "market_data_fetch", Some(symbol), json!({"source": src.name(), "error": format!("{e:#}")}), false)?;
                Err(e)
            }
        }
    }

    pub async fn ensure_samples(&self) -> Result<()> {
        for sym in market::SampleSource::symbols() {
            if market::read_meta(&self.home.data_dir(), sym).is_err() {
                self.fetch_data(&market::SampleSource, sym, 100_000).await?;
            }
        }
        Ok(())
    }

    // ---------------- backtest ----------------
    pub async fn backtest(&self, id: &str, symbol: &str, exec: &dyn SandboxExecutor, params_override: Option<serde_json::Value>) -> Result<BacktestSummary> {
        let s = self.store.get_strategy(id)?;
        let e = s.extracted.clone().context("strategy has no extraction")?;
        let code = e.code.clone().context("strategy has no code")?;
        let meta = market::read_meta(&self.home.data_dir(), symbol)?;
        self.transition(&s, StrategyStatus::Backtesting, "orchestrator", None, json!({"symbol": symbol, "backend": exec.name()}))?;
        let attempts = self.store.bump_attempts(&s.id)?;

        let job_id = uuid::Uuid::new_v4().to_string();
        let jd = self.home.job_dir(&s.id, &job_id);
        let (jin, jout) = (jd.join("job"), jd.join("out"));
        std::fs::create_dir_all(&jin)?;
        std::fs::create_dir_all(&jout)?;
        // container user 10001 must be able to write /out
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&jout, std::fs::Permissions::from_mode(0o777))?;
        }
        let mut params = serde_json::Value::Object(e.parameters.clone());
        if let Some(serde_json::Value::Object(o)) = params_override {
            for (k, v) in o {
                params[k] = v;
            }
        }
        std::fs::write(jin.join("strategy.py"), &code)?;
        let job_json = json!({"symbol": symbol, "data_file": format!("{symbol}.csv"), "params": params, "costs": self.costs,
            "split": {"train_frac": self.train_frac}, "data_provenance": meta});
        std::fs::write(jin.join("job.json"), serde_json::to_string_pretty(&job_json)?)?;

        let abs = |p: PathBuf| std::fs::canonicalize(&p).with_context(|| format!("canonicalize {}", p.display()));
        let mut mounts = vec![
            Mount { host: abs(self.home.data_dir())?, container: "/data".into(), read_only: true },
            Mount { host: abs(self.home.vendor_dir())?, container: "/vendor/huquant".into(), read_only: true },
            Mount { host: abs(jin.clone())?, container: "/job".into(), read_only: true },
            Mount { host: abs(jout.clone())?, container: "/out".into(), read_only: false },
        ];
        if !exec.hardened() {
            // process backend has no image; mount pinned runner script read-only
            mounts.push(Mount { host: abs(self.home.runner_dir())?, container: "/runner".into(), read_only: true });
        }
        let req = SandboxRequest { id: job_id.clone(), image: RUNNER_IMAGE.into(), command: RUNNER_CMD.iter().map(|s| s.to_string()).collect(),
            env: vec![], mounts, limits: self.limits.clone() };
        let job = JobRecord { id: job_id.clone(), strategy_id: s.id.clone(), backend: exec.name().into(),
            image: if exec.hardened() { RUNNER_IMAGE.into() } else { "host-python (dev)".into() }, symbol: symbol.into(),
            data_source: meta.source.clone(), status: "running".into(), exit_code: None, started_at: Utc::now(), finished_at: None,
            duration_ms: None, job_dir: jd.display().to_string(), error: None, result: None };
        self.store.insert_job(&job)?;
        self.store.audit("sandbox", "sandbox_start", Some(&s.id), json!({"job": job_id, "backend": exec.name(), "hardened": exec.hardened(),
            "limits": self.limits, "attempt": attempts, "symbol": symbol, "data_source": meta.source}), true)?;

        let s = self.store.get_strategy(&s.id)?;
        let res = exec.execute(&req).await;
        let fail = |job_status: &str, reason: String, exit: Option<i64>, ms: i64, result: Option<serde_json::Value>| -> Result<BacktestSummary> {
            self.store.finish_job(&job_id, job_status, exit, ms, Some(&reason), result.as_ref())?;
            self.store.audit("sandbox", "sandbox_finish", Some(&s.id), json!({"job": job_id, "status": job_status, "exit": exit, "error": reason}), false)?;
            self.transition(&s, StrategyStatus::Failed, "orchestrator", Some(&reason), json!({"job": job_id, "archived": jd.display().to_string()}))?;
            Ok(BacktestSummary { strategy_id: s.id.clone(), job_id: job_id.clone(), status: StrategyStatus::Failed, job_status: job_status.into(), reasons: vec![reason], oos: None })
        };
        let r = match res {
            Ok(r) => r,
            Err(e) => return fail("error", format!("sandbox error: {e}"), None, 0, None),
        };
        std::fs::write(jd.join("stdout.log"), &r.stdout)?;
        std::fs::write(jd.join("stderr.log"), &r.stderr)?;
        std::fs::write(jd.join("policy.json"), serde_json::to_string_pretty(&r.policy)?)?;
        let result: Option<serde_json::Value> = std::fs::read(jout.join("result.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let ms = r.duration_ms as i64;
        match r.outcome {
            Outcome::TimedOut => return fail("timeout", format!("sandbox timeout after {}s", self.limits.timeout_secs), Some(r.exit_code), ms, result),
            Outcome::OomKilled => return fail("oom", format!("out of memory (limit {} MB)", self.limits.memory_mb), Some(r.exit_code), ms, result),
            Outcome::Exited => {}
        }
        let status = result.as_ref().and_then(|v| v["status"].as_str().map(String::from)).unwrap_or_else(|| "no_result".into());
        if r.exit_code != 0 || status != "ok" {
            let why = match status.as_str() {
                "lookahead_detected" => "look-ahead bias detected by point-in-time probe".to_string(),
                _ => {
                    let err = result.as_ref().and_then(|v| v["error"].as_str()).map(String::from)
                        .unwrap_or_else(|| r.stderr.lines().last().unwrap_or("").to_string());
                    format!("runner exit {}: {}", r.exit_code, err.chars().take(300).collect::<String>())
                }
            };
            return fail(&status, why, Some(r.exit_code), ms, result);
        }
        let result = result.unwrap();
        self.store.finish_job(&job_id, "succeeded", Some(r.exit_code), ms, None, Some(&result))?;
        self.store.audit("sandbox", "sandbox_finish", Some(&s.id), json!({"job": job_id, "status": "succeeded", "ms": ms}), true)?;
        self.transition(&s, StrategyStatus::Evaluated, "orchestrator", None, json!({"job": job_id}))?;

        let g = gate::evaluate(&result, attempts, &self.gate);
        let ev = Evaluation { id: uuid::Uuid::new_v4().to_string(), strategy_id: s.id.clone(), job_id: job_id.clone(), passed: g.passed,
            reasons: g.reasons.clone(), checks: g.checks.clone(), created_at: Utc::now() };
        self.store.insert_evaluation(&ev)?;
        self.store.audit("gate", "evaluate", Some(&s.id), json!({"job": job_id, "passed": g.passed, "reasons": g.reasons, "attempts": attempts}), true)?;
        let s = self.store.get_strategy(&s.id)?;
        let final_status = if g.passed {
            self.store.set_status(&s.id, StrategyStatus::Evaluated, Some("gate passed — awaiting manual promote confirmation"))?;
            StrategyStatus::Evaluated
        } else {
            self.transition(&s, StrategyStatus::Rejected, "gate", Some(&g.reasons.join("; ")), json!({"job": job_id}))?;
            StrategyStatus::Rejected
        };
        Ok(BacktestSummary { strategy_id: s.id.clone(), job_id, status: final_status, job_status: "succeeded".into(), reasons: g.reasons, oos: Some(result["out_of_sample"].clone()) })
    }

    // ---------------- promote / reject (manual) ----------------
    /// Manual confirmation. `confirm` must equal the first 8 chars of the strategy id
    /// (typed confirmation, like CMspark's confirmation desk).
    pub fn promote(&self, id: &str, actor: &str, confirm: &str) -> Result<Strategy> {
        let s = self.store.get_strategy(id)?;
        if confirm != &s.id[..8] {
            self.store.audit(actor, "promote_denied", Some(&s.id), json!({"error": "confirmation token mismatch"}), false)?;
            bail!("confirmation mismatch: type the first 8 chars of the id ({})", &s.id[..8]);
        }
        let ev = self.store.latest_evaluation(&s.id)?;
        match ev {
            Some(ev) if ev.passed && s.status == StrategyStatus::Evaluated => {
                self.transition(&s, StrategyStatus::Promoted, actor, Some("manually confirmed"), json!({"evaluation": ev.id, "job": ev.job_id}))?;
                let job = self.store.list_jobs(Some(&s.id))?.into_iter().find(|j| j.id == ev.job_id);
                if let Some(r) = job.and_then(|j| j.result) {
                    let sig = Signal { strategy_id: s.id.clone(), job_id: ev.job_id.clone(), symbol: r["symbol"].as_str().unwrap_or("").into(),
                        date: r["latest_signal"]["date"].as_str().unwrap_or("").into(),
                        target_position: r["latest_signal"]["target_position"].as_f64().unwrap_or(0.0), created_at: Utc::now() };
                    self.store.insert_signal(&sig)?;
                    self.store.audit("system", "signal_observe_only", Some(&s.id), json!({"signal": sig, "note": "observe-only; no broker connected"}), true)?;
                }
                self.store.get_strategy(&s.id)
            }
            _ => {
                self.store.audit(actor, "promote_denied", Some(&s.id), json!({"status": s.status, "error": "latest evaluation did not pass the gate"}), false)?;
                bail!("cannot promote {}: status {} and latest evaluation must have passed the gate", s.id, s.status)
            }
        }
    }

    pub fn reject(&self, id: &str, actor: &str, reason: &str) -> Result<Strategy> {
        let s = self.store.get_strategy(id)?;
        self.transition(&s, StrategyStatus::Rejected, actor, Some(reason), json!({"manual": true}))?;
        self.store.get_strategy(&s.id)
    }
}
