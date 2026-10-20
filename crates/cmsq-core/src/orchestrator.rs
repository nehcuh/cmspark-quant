//! Core orchestrator: owns state transitions, audit emission and sandbox dispatch.
//! It never hands docker credentials to other components; the dashboard and CLI
//! only call these methods.
use crate::extract::Extractor;
use crate::gate::{self, GateConfig};
use crate::home::Home;
use crate::ingest::ScrapeDrop;
use crate::market::{self, MarketDataSource};
use crate::models::*;
use crate::error::CoreError;
use crate::market::Bar;
use crate::probe::{self, ProbeReport, Rng, Variant};
use crate::sandbox::{Mount, Outcome, SandboxExecutor, SandboxLimits, SandboxRequest, OUTPUT_CAP};
use crate::scorer::{self, Metrics};
use crate::state;
use crate::store::Store;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::json;
use sha2::Digest;
use std::path::{Path, PathBuf};

pub use crate::scorer::CostModel;

pub const RUNNER_IMAGE: &str = "cmsq-runner:2.0.0";
pub const RUNNER_CMD: [&str; 4] = ["python3", "-I", "-B", "/runner/signal_runner.py"];

/// Progress sink (dashboard log stream / CLI stderr). Called from the job thread.
pub type ProgressFn = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

pub struct Orchestrator {
    pub home: Home,
    pub store: Store,
    pub gate: GateConfig,
    pub costs: CostModel,
    /// First `dev_frac` of the bars is the development window (backtest + gate);
    /// the rest is the VAULT, only evaluated once, at promote time.
    pub dev_frac: f64,
    /// Leading fraction of the dev window treated as indicator warm-up (not scored).
    pub warmup_frac: f64,
    pub folds: usize,
    pub probe_cuts: Vec<f64>,
    pub probe_resamples: usize,
    pub limits: SandboxLimits,
    pub progress: Option<ProgressFn>,
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
        let store = Store::open(&home.db(), home.audit_config()?)?;
        let o = Self { home, store, gate: GateConfig::default(), costs: CostModel::default(), dev_frac: 0.8, warmup_frac: 0.2, folds: 4,
            probe_cuts: vec![0.5, 0.7, 0.9], probe_resamples: 2, limits: SandboxLimits::default(), progress: None };
        o.reap_stale_jobs()?;
        Ok(o)
    }

    fn log(&self, msg: &str) {
        if let Some(p) = &self.progress {
            p(msg);
        }
    }

    /// Crash recovery: jobs left `running` by a dead process are marked
    /// `abandoned`, and strategies stuck in Backtesting without a live job go to Failed.
    pub fn reap_stale_jobs(&self) -> Result<Vec<String>> {
        let mut reaped = vec![];
        for j in self.store.stale_running_jobs()? {
            self.store.finish_job(&j.id, "abandoned", None, 0, Some("owner process exited before the job finished (crash recovery)"), None)?;
            self.store.audit("orchestrator", "job_reaped", Some(&j.strategy_id), json!({"job": j.id, "owner_pid": j.owner_pid}), true)?;
            reaped.push(j.id);
        }
        let live: Vec<String> = self.store.list_jobs(None)?.into_iter().filter(|j| j.status == "running").map(|j| j.strategy_id).collect();
        for s in self.store.list_strategies(Some(StrategyStatus::Backtesting))? {
            if !live.contains(&s.id) {
                self.transition(&s, StrategyStatus::Failed, "orchestrator", Some("recovered: backtest was interrupted (process crashed or was killed)"), json!({"recovery": true}))?;
                reaped.push(s.id);
            }
        }
        Ok(reaped)
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
                if p.extension().is_some_and(|x| x == "json") {
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
            // never follow symlinks / read FIFOs or devices out of the drop folder
            let meta = std::fs::symlink_metadata(&f)?;
            let o = if !meta.file_type().is_file() {
                self.store.audit("ingest", "ingest_rejected", None, json!({"origin": f.display().to_string(), "error": "not a regular file (symlink/fifo/device)"}), false)?;
                IngestOutcome::Invalid("not a regular file (symlinks are not followed)".into())
            } else if meta.len() > crate::ingest::MAX_DROP_BYTES {
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
        let s = self.get(id)?;
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
                let meta = market::read_meta(&self.home.data_dir(), symbol)?;
                self.store.audit("data", "market_data_fetch", Some(symbol), json!({"source": src.name(), "meta": meta}), true)?;
                Ok(meta)
            }
            Err(e) => {
                self.store.audit("data", "market_data_fetch", Some(symbol), json!({"source": src.name(), "error": format!("{e:#}")}), false)?;
                Err(e)
            }
        }
    }

    /// Fetch through the multi-source router (priority + fallback + cross-validation).
    pub async fn fetch_routed(&self, router: &crate::data::router::Router, symbol: &str, count: usize) -> Result<market::DataMeta> {
        match router.fetch(symbol, count).await {
            Ok((bars, meta)) => {
                market::write_cache(&self.home.data_dir(), &bars, &meta)?;
                let meta = market::read_meta(&self.home.data_dir(), symbol)?;
                self.store.audit("data", "market_data_fetch", Some(symbol), json!({"source": meta.source, "router": true, "snapshot_sha256": meta.extra["snapshot_sha256"],
                    "validation": meta.extra["validation"], "attempts": meta.extra["router"], "bars": meta.bars, "start": meta.start, "end": meta.end, "adjustment": meta.adjustment}), true)?;
                Ok(meta)
            }
            Err(e) => {
                self.store.audit("data", "market_data_fetch", Some(symbol), json!({"router": true, "error": format!("{e:#}")}), false)?;
                Err(e)
            }
        }
    }

    pub fn data_router(&self, list: Option<&str>, validate: bool) -> (crate::data::router::Router, Vec<String>) {
        let (sources, notes) = crate::data::build_sources(list, &self.home.secrets);
        (crate::data::router::Router { sources, health_file: self.home.data_dir().join("_sources_health.json"), validate }, notes)
    }

    pub async fn ensure_samples(&self) -> Result<()> {
        for sym in market::SampleSource::symbols() {
            if market::read_meta(&self.home.data_dir(), sym).is_err() {
                self.fetch_data(&market::SampleSource, sym, 100_000).await?;
            }
        }
        Ok(())
    }

        pub fn get(&self, id: &str) -> Result<Strategy> {
        self.store.get_strategy(id).map_err(|e| if e.to_string().starts_with("strategy not found") { CoreError::NotFound(id.into()).into() } else { e })
    }

    /// The exact text a human must type to promote (never pre-filled by any UI).
    pub fn confirmation_phrase(s: &Strategy) -> String {
        format!("PROMOTE {}", &s.id[..8])
    }

    // ---------------- sandbox: one variant per fresh container ----------------
    #[allow(clippy::too_many_arguments)]
    async fn run_variant(&self, exec: &dyn SandboxExecutor, jd: &Path, idx: usize, v: &Variant, code: &str, params: &serde_json::Value)
        -> std::result::Result<(Vec<f64>, u64), (String, String)> {
        let io = |e: std::io::Error| ("error".to_string(), format!("host io: {e}"));
        let vd = jd.join("variants").join(format!("{idx:02}"));
        let (vin, vjob, vout) = (vd.join("input"), vd.join("job"), vd.join("out"));
        for d in [&vin, &vjob, &vout] {
            std::fs::create_dir_all(d).map_err(io)?;
        }
        std::fs::write(vin.join("bars.csv"), market::to_csv(&v.bars, false)).map_err(io)?;
        std::fs::write(vjob.join("strategy.py"), code).map_err(io)?;
        std::fs::write(vjob.join("job.json"), json!({"params": params}).to_string()).map_err(io)?;
        let abs = |p: &Path| std::fs::canonicalize(p).map_err(io);
        let mut mounts = vec![
            Mount { host: abs(&vin)?, container: "/input".into(), read_only: true },
            Mount { host: abs(&vjob)?, container: "/job".into(), read_only: true },
            Mount { host: abs(&vout)?, container: "/out".into(), read_only: false },
        ];
        if !exec.hardened() {
            mounts.push(Mount { host: abs(&self.home.runner_dir())?, container: "/runner".into(), read_only: true });
        }
        let req = SandboxRequest { id: format!("{}-{idx:02}", jd.file_name().unwrap().to_string_lossy()), image: RUNNER_IMAGE.into(),
            command: RUNNER_CMD.iter().map(|s| s.to_string()).collect(), env: vec![], mounts, limits: self.limits.clone() };
        let r = exec.execute(&req).await.map_err(|e| ("error".to_string(), format!("sandbox error: {e}")))?;
        let _ = std::fs::write(vd.join("stdout.log"), &r.stdout);
        let _ = std::fs::write(vd.join("stderr.log"), &r.stderr);
        let _ = std::fs::write(vd.join("policy.json"), r.policy.to_string());
        let last = || r.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(300).collect::<String>();
        match r.outcome {
            Outcome::TimedOut => return Err(("timeout".into(), format!("sandbox timeout after {}s (variant {})", self.limits.timeout_secs, v.name))),
            Outcome::OomKilled => return Err(("oom".into(), format!("out of memory (limit {} MB)", self.limits.memory_mb))),
            Outcome::Exited => {}
        }
        if r.stdout.len() >= OUTPUT_CAP || r.stderr.len() >= OUTPUT_CAP {
            return Err(("output_cap".into(), format!("output exceeded {} MiB cap; job killed from scoring", OUTPUT_CAP >> 20)));
        }
        let pids = ["Resource temporarily unavailable", "can't start new thread", "BlockingIOError"];
        if r.exit_code != 0 {
            if pids.iter().any(|p| r.stderr.contains(p)) {
                return Err(("pids_exhausted".into(), format!("process limit ({} pids) exhausted: {}", self.limits.pids, last())));
            }
            return Err(("strategy_error".into(), format!("runner exit {}: {}", r.exit_code, last())));
        }
        // untrusted output: regular file only (no symlink), size-capped, strict parse
        let p = vout.join("positions.csv");
        let md = std::fs::symlink_metadata(&p).map_err(|_| ("bad_output".to_string(), "no positions.csv".to_string()))?;
        if !md.file_type().is_file() || md.len() > 8 * 1024 * 1024 {
            return Err(("bad_output".into(), "positions.csv is not a regular file under 8 MiB".into()));
        }
        let text = {
            use std::io::Read;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc_o_nofollow()).open(&p).map_err(io)?;
            let mut s = String::new();
            f.by_ref().take(8 * 1024 * 1024).read_to_string(&mut s).map_err(|_| ("bad_output".to_string(), "positions.csv is not UTF-8".to_string()))?;
            s
        };
        let dates: Vec<String> = v.bars.iter().map(|b| b.date.clone()).collect();
        let pos = probe::parse_positions(&text, &dates).map_err(|e| ("bad_output".to_string(), e))?;
        Ok((pos, r.duration_ms))
    }

    /// Runs a shuffled probe plan, one fresh container per variant.
    async fn run_plan(&self, exec: &dyn SandboxExecutor, jd: &Path, plan: &[Variant], code: &str, params: &serde_json::Value)
        -> std::result::Result<(Vec<f64>, ProbeReport, u64), (String, String)> {
        let mut results: Vec<(usize, Vec<f64>)> = vec![];
        let mut total_ms = 0;
        for (i, v) in plan.iter().enumerate() {
            self.log(&format!("container {}/{}: variant {} ({} bars)", i + 1, plan.len(), v.name, v.bars.len()));
            let (pos, ms) = self.run_variant(exec, jd, i, v, code, params).await?;
            total_ms += ms;
            results.push((i, pos));
        }
        let fi = plan.iter().position(|v| v.name == "full").expect("plan has full");
        let full = results.iter().find(|r| r.0 == fi).unwrap().1.clone();
        let dates: Vec<String> = plan[fi].bars.iter().map(|b| b.date.clone()).collect();
        let mismatches: Vec<serde_json::Value> = results.iter().filter(|r| r.0 != fi).filter_map(|(i, p)| probe::compare(&full, &plan[*i], p, &dates)).collect();
        let rep = ProbeReport { detected: !mismatches.is_empty(), variants: plan.len(), containers: results.len(),
            order: plan.iter().map(|v| v.name.clone()).collect(), mismatches };
        Ok((full, rep, total_ms))
    }

    fn params_for(e: &ExtractedStrategy, over: Option<serde_json::Value>) -> serde_json::Value {
        let mut params = serde_json::Value::Object(e.parameters.clone());
        if let Some(serde_json::Value::Object(o)) = over {
            for (k, v) in o {
                params[k] = v;
            }
        }
        params
    }

    fn write_equity(path: &Path, bars: &[Bar], pos: &[f64], sim: &scorer::SimResult, sim2: &scorer::SimResult) -> Result<()> {
        let mut s = String::from("date,equity,equity_cost2x,target_position,exposure\n");
        for i in 0..bars.len() {
            s.push_str(&format!("{},{:.2},{:.2},{:.6},{:.6}\n", bars[i].date, sim.equity[i], sim2.equity[i], pos[i], sim.exposure[i]));
        }
        std::fs::write(path, s)?;
        Ok(())
    }

    // ---------------- backtest (development window) ----------------
    pub async fn backtest(&self, id: &str, symbol: &str, exec: &dyn SandboxExecutor, params_override: Option<serde_json::Value>) -> Result<BacktestSummary> {
        let s = self.get(id)?;
        if let Err(e) = state::check(s.status, StrategyStatus::Backtesting) {
            self.store.audit("orchestrator", "transition_denied", Some(&s.id), json!({"from": s.status, "to": "Backtesting", "error": e.to_string()}), false)?;
            return Err(e);
        }
        let e = s.extracted.clone().ok_or_else(|| CoreError::NoCode(s.id.clone()))?;
        let code = e.code.clone().ok_or_else(|| CoreError::NoCode(s.id.clone()))?;
        let meta = market::read_meta(&self.home.data_dir(), symbol).map_err(|_| CoreError::NoData(symbol.into()))?;
        let bars = market::read_bars(&self.home.data_dir(), symbol)?;
        anyhow::ensure!(bars.len() >= 300, "need at least 300 bars, {symbol} has {}", bars.len());
        let dataset_sha = hex::encode(sha2::Sha256::digest(market::to_csv(&bars, true).as_bytes()));
        if let Some(snap) = meta.extra.get("snapshot_sha256").and_then(|v| v.as_str()) {
            anyhow::ensure!(snap == dataset_sha, "data cache for {symbol} was modified after fetch (snapshot {snap}, now {dataset_sha}); re-fetch it");
        }
        let dev_end = ((bars.len() as f64) * self.dev_frac) as usize;
        let dev = &bars[..dev_end];
        self.transition(&s, StrategyStatus::Backtesting, "orchestrator", None, json!({"symbol": symbol, "backend": exec.name()}))?;
        let attempts = self.store.bump_attempts(&s.id)?;
        let params = Self::params_for(&e, params_override);
        let identity = exec.runner_identity(RUNNER_IMAGE).await;

        let job_id = uuid::Uuid::new_v4().to_string();
        let jd = self.home.job_dir(&s.id, &job_id);
        std::fs::create_dir_all(&jd)?;
        let job = JobRecord { id: job_id.clone(), strategy_id: s.id.clone(), backend: exec.name().into(),
            image: if exec.hardened() { RUNNER_IMAGE.into() } else { "host-python (dev)".into() }, symbol: symbol.into(),
            data_source: meta.source.clone(), status: "running".into(), exit_code: None, started_at: Utc::now(), finished_at: None,
            duration_ms: None, job_dir: jd.display().to_string(), error: None, result: None, owner_pid: None,
            runner_identity: identity.clone(), kind: "dev".into() };
        self.store.insert_job(&job)?;
        let mut rng = Rng::from_uuid();
        let view = probe::container_view(dev);
        let plan = probe::plan(&view, &self.probe_cuts, self.probe_resamples, scorer::limit_pct(symbol, &dev[0].date) - 0.005, &mut rng);
        self.store.audit("sandbox", "sandbox_start", Some(&s.id), json!({"job": job_id, "backend": exec.name(), "hardened": exec.hardened(),
            "runner_identity": identity, "limits": self.limits, "attempt": attempts, "symbol": symbol, "data_source": meta.source,
            "dev_window": {"start": dev[0].date, "end": dev[dev_end - 1].date, "bars": dev_end}, "vault_bars": bars.len() - dev_end,
            "variants": plan.iter().map(|v| &v.name).collect::<Vec<_>>()}), true)?;
        self.log(&format!("job {} started: {} variants, one fresh container each, random order", &job_id[..8], plan.len()));

        let s = self.get(&s.id)?;
        let t0 = std::time::Instant::now();
        let (full, probe_rep, _) = match self.run_plan(exec, &jd, &plan, &code, &params).await {
            Ok(x) => x,
            Err((status, reason)) => {
                let ms = t0.elapsed().as_millis() as i64;
                self.store.finish_job(&job_id, &status, None, ms, Some(&reason), None)?;
                self.store.audit("sandbox", "sandbox_finish", Some(&s.id), json!({"job": job_id, "status": status, "error": reason}), false)?;
                self.transition(&s, StrategyStatus::Failed, "orchestrator", Some(&reason), json!({"job": job_id, "status": status}))?;
                self.log(&format!("FAILED ({status}): {reason}"));
                return Ok(BacktestSummary { strategy_id: s.id.clone(), job_id, status: StrategyStatus::Failed, job_status: status, reasons: vec![reason], oos: None });
            }
        };
        let ms = t0.elapsed().as_millis() as i64;
        self.log("all containers finished; scoring on host (trusted Rust)");

        // ---- trusted scoring ----
        let sim = scorer::simulate(symbol, dev, &full, &self.costs);
        let sim2 = scorer::simulate(symbol, dev, &full, &self.costs.stressed(2.0));
        let warm = ((dev_end as f64) * self.warmup_frac) as usize;
        let dm = scorer::metrics(dev, &sim, warm, dev_end);
        let dm2 = scorer::metrics(dev, &sim2, warm, dev_end);
        let fold_len = (dev_end - warm) / self.folds.max(1);
        let folds: Vec<Metrics> = (0..self.folds).map(|k| scorer::metrics(dev, &sim, warm + k * fold_len, if k + 1 == self.folds { dev_end } else { warm + (k + 1) * fold_len })).collect();
        let bh = scorer::simulate(symbol, dev, &vec![1.0; dev_end], &self.costs);
        let bhm = scorer::metrics(dev, &bh, warm, dev_end);
        Self::write_equity(&jd.join("equity.csv"), dev, &full, &sim, &sim2)?;

        // ---- library-wide trial ledger + DSR ----
        let ikey = crate::extract::idea_key(&code);
        let phash = hex::encode(&sha2::Sha256::digest(params.to_string().as_bytes())[..8]);
        let prior = self.store.trials()?;
        let near_dups: Vec<String> = prior.iter().filter(|t| t.strategy_id != s.id && t.symbol == symbol && t.positions.len() == full.len())
            .filter(|t| t.idea_key == ikey || corr(&t.positions, &full).is_some_and(|c| c > 0.9))
            .map(|t| t.strategy_id.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        self.store.insert_trial(&s.id, &job_id, &ikey, &phash, symbol, &dataset_sha, dm.bars, dm.sharpe_daily, &full, probe_rep.detected)?;
        let all = self.store.trials()?;
        let srs: Vec<f64> = all.iter().filter(|t| !t.lookahead).map(|t| t.sharpe_daily).collect();
        let dsr = scorer::deflated_sharpe_n(&dm, &srs, all.len());

        let high: Vec<String> = e.risks.iter().filter(|r| r.severity == Severity::High && crate::extract::BLOCKING_RISKS.contains(&r.code.as_str())).map(|r| r.code.clone()).collect();
        let adjusted = market::is_adjusted(&meta, &bars);
        let validated = crate::data::router::validation_status(&meta);
        let result = json!({
            "schema_version": 2, "computed_by": "cmsq-core scorer (host, trusted)", "symbol": symbol, "execution": scorer::EXECUTION_LABEL,
            "costs": self.costs, "initial_capital": scorer::INITIAL_CAPITAL, "runner_identity": identity, "backend": exec.name(), "hardened": exec.hardened(),
            "data_provenance": meta, "dataset_sha256": dataset_sha, "data_adjusted": adjusted, "data_cross_validation": {"ok": validated.0, "detail": validated.1},
            "split": {"development": {"start": dev[0].date, "end": dev[dev_end - 1].date, "bars": dev_end, "warmup_bars": warm},
                      "vault": {"start": bars[dev_end].date, "end": bars.last().unwrap().date, "bars": bars.len() - dev_end, "note": "never shown to the strategy before promote"}},
            "development": dm, "development_cost_2x": dm2, "walk_forward_folds": folds, "buy_and_hold": bhm,
            "trades": sim.trades, "blocked": {"limit_up": sim.blocked_limit_up, "limit_down": sim.blocked_limit_down, "suspended": sim.blocked_suspended},
            "costs_paid": sim.costs_paid, "lookahead_probe": probe_rep, "trial": {"idea_key": ikey, "params_hash": phash, "library_trials": all.len(), "excluded_from_variance_lookahead": all.len() - srs.len(), "near_duplicates_of": near_dups},
            "dsr": dsr, "latest_position": {"date": dev[dev_end - 1].date, "target_position": full[dev_end - 1]}, "equity_csv": jd.join("equity.csv").display().to_string(),
        });
        std::fs::write(jd.join("result.json"), serde_json::to_string_pretty(&result)?)?;
        self.store.finish_job(&job_id, "succeeded", Some(0), ms, None, Some(&result))?;
        self.store.audit("sandbox", "sandbox_finish", Some(&s.id), json!({"job": job_id, "status": "succeeded", "ms": ms, "containers": probe_rep.containers}), true)?;
        self.transition(&s, StrategyStatus::Evaluated, "orchestrator", None, json!({"job": job_id}))?;

        let g = gate::evaluate(&gate::GateInput { job_status: "succeeded", hardened: exec.hardened(), runner_identity: identity.as_deref(), dev: &dm,
            dev_cost_2x: &dm2, folds: &folds, dsr: &dsr, probe: &probe_rep, data_adjusted: adjusted, data_validated: validated.clone(), high_risks: &high }, &self.gate);
        let ev = Evaluation { id: uuid::Uuid::new_v4().to_string(), strategy_id: s.id.clone(), job_id: job_id.clone(), passed: g.passed,
            reasons: g.reasons.clone(), checks: g.checks.clone(), created_at: Utc::now() };
        self.store.insert_evaluation(&ev)?;
        self.store.audit("gate", "evaluate", Some(&s.id), json!({"job": job_id, "passed": g.passed, "reasons": g.reasons, "attempts": attempts, "dsr": dsr.dsr, "trials": dsr.trials}), true)?;
        let s = self.get(&s.id)?;
        let final_status = if g.passed {
            self.transition(&s, StrategyStatus::GatePassed, "gate", Some("gate passed on development data — vault holdout runs at manual promote"), json!({"job": job_id}))?;
            StrategyStatus::GatePassed
        } else {
            self.transition(&s, StrategyStatus::Rejected, "gate", Some(&g.reasons.join("; ")), json!({"job": job_id}))?;
            StrategyStatus::Rejected
        };
        self.log(&format!("{final_status}: Sharpe {:.2}, DSR {:.3} (N={}), {}/{} folds positive", dm.sharpe, dsr.dsr, dsr.trials,
            folds.iter().filter(|f| f.total_return > 0.0).count(), folds.len()));
        Ok(BacktestSummary { strategy_id: s.id.clone(), job_id, status: final_status, job_status: "succeeded".into(), reasons: g.reasons, oos: Some(json!(dm)) })
    }

    // ---------------- promote (manual + one-time vault holdout) / reject ----------------
    /// `confirm` must be typed by a human: exactly `PROMOTE <first 8 chars of id>`.
    /// Then the strategy is run ONCE on the vault (bars never shown before). A vault
    /// failure rejects; the vault can't be retried for the same strategy.
    pub async fn promote(&self, id: &str, actor: &str, confirm: &str, exec: &dyn SandboxExecutor) -> Result<Strategy> {
        let s = self.get(id)?;
        let phrase = Self::confirmation_phrase(&s);
        if confirm.trim() != phrase {
            self.store.audit(actor, "promote_denied", Some(&s.id), json!({"error": "confirmation phrase mismatch"}), false)?;
            return Err(CoreError::ConfirmationMismatch { expected: phrase }.into());
        }
        let ev = self.store.latest_evaluation(&s.id)?;
        let ev = match ev {
            Some(ev) if ev.passed && s.status == StrategyStatus::GatePassed => ev,
            _ => {
                self.store.audit(actor, "promote_denied", Some(&s.id), json!({"status": s.status, "error": "not GatePassed"}), false)?;
                return Err(CoreError::NotGatePassed { id: s.id.clone(), status: s.status }.into());
            }
        };
        if let Some((job, _)) = self.store.vault_touch(&s.id)? {
            self.store.audit(actor, "promote_denied", Some(&s.id), json!({"error": "vault already used", "vault_job": job}), false)?;
            self.transition(&s, StrategyStatus::Rejected, actor, Some("vault holdout already used by an earlier promote attempt"), json!({}))?;
            return self.get(&s.id);
        }
        if !exec.hardened() && self.gate.require_hardened_backend {
            return Err(CoreError::BackendUnavailable("promotion (vault run) requires the Docker backend".into()).into());
        }
        let dev_job = self.store.list_jobs(Some(&s.id))?.into_iter().find(|j| j.id == ev.job_id).context("evaluation job missing")?;
        let symbol = dev_job.symbol.clone();
        let e = s.extracted.clone().ok_or_else(|| CoreError::NoCode(s.id.clone()))?;
        let code = e.code.clone().ok_or_else(|| CoreError::NoCode(s.id.clone()))?;
        let bars = market::read_bars(&self.home.data_dir(), &symbol)?;
        let dev_end = ((bars.len() as f64) * self.dev_frac) as usize;
        let identity = exec.runner_identity(RUNNER_IMAGE).await;
        let job_id = uuid::Uuid::new_v4().to_string();
        let jd = self.home.job_dir(&s.id, &job_id);
        std::fs::create_dir_all(&jd)?;
        self.store.insert_job(&JobRecord { id: job_id.clone(), strategy_id: s.id.clone(), backend: exec.name().into(), image: RUNNER_IMAGE.into(),
            symbol: symbol.clone(), data_source: dev_job.data_source.clone(), status: "running".into(), exit_code: None, started_at: Utc::now(),
            finished_at: None, duration_ms: None, job_dir: jd.display().to_string(), error: None, result: None, owner_pid: None,
            runner_identity: identity.clone(), kind: "vault".into() })?;
        self.store.record_vault_touch(&s.id, &job_id, false)?; // recorded BEFORE running: a crash still burns the vault
        self.store.audit(actor, "vault_start", Some(&s.id), json!({"job": job_id, "confirmed_by": actor, "vault": {"start": bars[dev_end].date, "end": bars.last().unwrap().date}}), true)?;
        // variants: full series, the exact dev window (consistency), and cuts inside the vault
        let view = probe::container_view(&bars);
        let n = bars.len();
        let mut rng = Rng::from_uuid();
        let lim = scorer::limit_pct(&symbol, &bars[dev_end].date) - 0.005;
        let cuts = [dev_end as f64 / n as f64, (dev_end + (n - dev_end) / 2) as f64 / n as f64];
        let plan = probe::plan(&view, &cuts, 1, lim, &mut rng);
        let params = Self::params_for(&e, None);
        let t0 = std::time::Instant::now();
        let outcome = self.run_plan(exec, &jd, &plan, &code, &params).await;
        let ms = t0.elapsed().as_millis() as i64;
        let s = self.get(&s.id)?;
        let (full, rep) = match outcome {
            Ok((f, r, _)) => (f, r),
            Err((status, reason)) => {
                self.store.finish_job(&job_id, &status, None, ms, Some(&reason), None)?;
                self.transition(&s, StrategyStatus::Rejected, "gate", Some(&format!("vault run failed ({status}): {reason}")), json!({"job": job_id}))?;
                return self.get(&s.id);
            }
        };
        let sim = scorer::simulate(&symbol, &bars, &full, &self.costs);
        let vm = scorer::metrics(&bars, &sim, dev_end, n);
        Self::write_equity(&jd.join("equity.csv"), &bars, &full, &sim, &scorer::simulate(&symbol, &bars, &full, &self.costs.stressed(2.0)))?;
        let mut reasons = vec![];
        if rep.detected {
            reasons.push(format!("vault look-ahead/consistency probe failed: {:?}", rep.mismatches.first()));
        }
        if vm.total_return <= 0.0 || vm.sharpe <= 0.0 {
            reasons.push(format!("vault holdout not profitable: return {:.2}%, Sharpe {:.2}", vm.total_return * 100.0, vm.sharpe));
        }
        if !gate::runner_version_ok(identity.as_deref()) {
            reasons.push("runner identity not pinned".into());
        }
        let passed = reasons.is_empty();
        let result = json!({"kind": "vault", "computed_by": "cmsq-core scorer (host, trusted)", "symbol": symbol, "vault": vm, "lookahead_probe": rep,
            "runner_identity": identity, "execution": scorer::EXECUTION_LABEL, "passed": passed, "reasons": reasons,
            "latest_position": {"date": bars[n - 1].date, "target_position": full[n - 1]}});
        std::fs::write(jd.join("result.json"), serde_json::to_string_pretty(&result)?)?;
        self.store.finish_job(&job_id, "succeeded", Some(0), ms, None, Some(&result))?;
        self.store.conn.execute("UPDATE vault_touches SET passed=?2 WHERE strategy_id=?1", rusqlite::params![s.id, passed as i64])?;
        self.store.audit("gate", "vault_evaluate", Some(&s.id), json!({"job": job_id, "passed": passed, "reasons": reasons, "vault": vm}), true)?;
        if !passed {
            self.transition(&s, StrategyStatus::Rejected, "gate", Some(&reasons.join("; ")), json!({"job": job_id}))?;
            return self.get(&s.id);
        }
        self.transition(&s, StrategyStatus::Promoted, actor, Some("manually confirmed; vault holdout passed"), json!({"evaluation": ev.id, "dev_job": ev.job_id, "vault_job": job_id}))?;
        let sig = Signal { strategy_id: s.id.clone(), job_id: job_id.clone(), symbol: symbol.clone(), date: bars[n - 1].date.clone(), target_position: full[n - 1], created_at: Utc::now() };
        self.store.insert_signal(&sig)?;
        self.store.audit("system", "signal_observe_only", Some(&s.id), json!({"signal": sig, "note": "observe-only; no broker connected"}), true)?;
        self.get(&s.id)
    }

    pub fn reject(&self, id: &str, actor: &str, reason: &str) -> Result<Strategy> {
        let s = self.get(id)?;
        self.transition(&s, StrategyStatus::Rejected, actor, Some(reason), json!({"manual": true}))?;
        self.get(&s.id)
    }
}

fn corr(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len().min(b.len()) as f64;
    if n < 3.0 {
        return None;
    }
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma).powi(2);
        sbb += (y - mb).powi(2);
    }
    if saa < 1e-12 || sbb < 1e-12 {
        return None;
    }
    Some(sab / (saa * sbb).sqrt())
}

const fn libc_o_nofollow() -> i32 {
    0o400000 // O_NOFOLLOW on Linux (x86_64/aarch64)
}
