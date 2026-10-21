//! Core orchestrator: owns state transitions, audit emission and sandbox dispatch.
//! It never hands docker credentials to other components; the dashboard and CLI
//! only call these methods.
use crate::error::CoreError;
use crate::extract::Extractor;
use crate::gate::{self, GateConfig};
use crate::home::Home;
use crate::ingest::ScrapeDrop;
use crate::market::Bar;
use crate::market::{self, MarketDataSource};
use crate::models::*;
use crate::probe::{self, ProbeReport, Rng, Variant};
use crate::sandbox::{Mount, Outcome, SandboxExecutor, SandboxLimits, SandboxRequest, OUTPUT_CAP};
use crate::scorer::{self, Metrics};
use crate::state;
use crate::store::{NewTrial, Store};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::json;
use sha2::Digest;
use std::path::{Path, PathBuf};

pub use crate::scorer::CostModel;

pub const RUNNER_IMAGE: &str = "cmsq-runner:2.2.2";
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
    pub probe: probe::ProbeConfig,
    /// parallel containers per job (each still a fresh container)
    pub parallel: usize,
    pub limits: SandboxLimits,
    pub progress: Option<ProgressFn>,
    /// global container slots shared by every concurrent job of this process
    /// (Docker capacity: `CMSQ_CONTAINER_SLOTS`, default = CPU count)
    pub slots: std::sync::Arc<tokio::sync::Semaphore>,
}

pub fn default_slots() -> usize {
    std::env::var("CMSQ_CONTAINER_SLOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
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
        let store = Store::open_configured(&home.db(), &home.db_config(), home.audit_config()?)?;
        Self::with_store(home, store)
    }

    /// Open with an explicit store (tests: SQLite file / dedicated PostgreSQL schema).
    pub fn with_store(home: Home, store: Store) -> Result<Self> {
        home.init()?;
        let o = Self {
            home,
            store,
            gate: GateConfig::default(),
            costs: CostModel::default(),
            dev_frac: 0.8,
            warmup_frac: 0.2,
            folds: 4,
            probe: probe::ProbeConfig::default(),
            parallel: 4,
            limits: SandboxLimits::default(),
            progress: None,
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(default_slots())),
        };
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
            if self.store.abandon_job(
                &j.id,
                "owner process exited before the job finished (crash recovery)",
            )? {
                self.store.audit(
                    "orchestrator",
                    "job_reaped",
                    Some(&j.strategy_id),
                    json!({"job": j.id, "owner_pid": j.owner_pid}),
                    true,
                )?;
                reaped.push(j.id);
            }
        }
        // A Backtesting strategy is recovered only when its run lease is provably
        // dead on this host (issue #8); a live owner between status change and job
        // insert, or between sweep children, is never touched. Legacy rows without
        // a lease fall back to "no running job".
        let live: Vec<String> = self
            .store
            .list_jobs(None)?
            .into_iter()
            .filter(|j| j.status == "running")
            .map(|j| j.strategy_id)
            .collect();
        for s in self
            .store
            .list_strategies(Some(StrategyStatus::Backtesting))?
        {
            let lease = self.store.run_owner(&s.id)?;
            let dead = match lease.as_deref() {
                Some(l) => crate::store::lease_alive(l) == Some(false),
                None => !live.contains(&s.id),
            };
            if !dead {
                continue;
            }
            let why = "recovered: backtest was interrupted (owner process exited)";
            if self.store.reap_backtesting(&s.id, lease.as_deref(), why)? {
                self.store.audit(
                    "orchestrator",
                    "transition:Backtesting->Failed",
                    Some(&s.id),
                    json!({"reason": why, "details": {"recovery": true, "lease": lease}}),
                    true,
                )?;
                reaped.push(s.id);
            }
        }
        Ok(reaped)
    }

    fn transition(
        &self,
        s: &Strategy,
        to: StrategyStatus,
        actor: &str,
        reason: Option<&str>,
        details: serde_json::Value,
    ) -> Result<()> {
        if let Err(e) = state::check(s.status, to) {
            self.store.audit(
                actor,
                "transition_denied",
                Some(&s.id),
                json!({"from": s.status, "to": to, "error": e.to_string()}),
                false,
            )?;
            return Err(e);
        }
        if !self.store.cas_status(&s.id, s.status, to, reason)? {
            let cur = self
                .store
                .get_strategy(&s.id)
                .map(|x| x.status.to_string())
                .unwrap_or_default();
            self.store.audit(
                actor,
                "transition_conflict",
                Some(&s.id),
                json!({"from": s.status, "to": to, "actual": cur}),
                false,
            )?;
            return Err(CoreError::Conflict(format!(
                "{} moved to {cur} concurrently (wanted {} -> {to})",
                &s.id[..8.min(s.id.len())],
                s.status
            ))
            .into());
        }
        self.store.audit(
            actor,
            &format!("transition:{}->{}", s.status, to),
            Some(&s.id),
            json!({"reason": reason, "details": details}),
            true,
        )?;
        Ok(())
    }

    // ---------------- ingest ----------------
    pub fn ingest_bytes(&self, bytes: &[u8], origin: &str) -> Result<IngestOutcome> {
        let d = match ScrapeDrop::parse(bytes) {
            Ok(d) => d,
            Err(e) => {
                self.store.audit(
                    "ingest",
                    "ingest_rejected",
                    None,
                    json!({"origin": origin, "error": format!("{e:#}")}),
                    false,
                )?;
                return Ok(IngestOutcome::Invalid(format!("{e:#}")));
            }
        };
        let sha = d.content_sha256();
        if let Some(id) = self.store.find_by_sha(&sha)? {
            self.store.audit(
                "ingest",
                "ingest_duplicate",
                Some(&id),
                json!({"origin": origin}),
                true,
            )?;
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
            id: id.clone(),
            title: d.content.title.chars().take(200).collect(),
            source_site: d.source.site.clone(),
            source_url: d.source.url.clone(),
            author: d.source.author.clone(),
            scraped_at: Some(d.scraped_at.clone()),
            ingested_at: now,
            raw_path: raw.display().to_string(),
            content_sha256: sha.clone(),
            sample: d.sample,
            status: StrategyStatus::Ingested,
            attempts: 0,
            last_reason: None,
            extracted: None,
            updated_at: now,
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
        let in_drop = std::fs::canonicalize(path)
            .ok()
            .map(|p| p.starts_with(std::fs::canonicalize(self.home.drop_dir()).unwrap_or_default()))
            .unwrap_or(false);
        let mut out = vec![];
        for f in files {
            // never follow symlinks / read FIFOs or devices out of the drop folder
            let meta = std::fs::symlink_metadata(&f)?;
            let o = if !meta.file_type().is_file() {
                self.store.audit("ingest", "ingest_rejected", None, json!({"origin": f.display().to_string(), "error": "not a regular file (symlink/fifo/device)"}), false)?;
                IngestOutcome::Invalid("not a regular file (symlinks are not followed)".into())
            } else if meta.len() > crate::ingest::MAX_DROP_BYTES {
                self.store.audit(
                    "ingest",
                    "ingest_rejected",
                    None,
                    json!({"origin": f.display().to_string(), "error": "too large"}),
                    false,
                )?;
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
            bail!(
                "strategy {} is {}, extract requires Ingested",
                s.id,
                s.status
            );
        }
        let post = self.load_post(&s)?;
        let e = match ex.extract(&post).await {
            Ok(e) => e,
            Err(err) => {
                self.transition(
                    &s,
                    StrategyStatus::Failed,
                    "extractor",
                    Some(&format!("extraction error: {err:#}")),
                    json!({}),
                )?;
                return self.store.get_strategy(&s.id);
            }
        };
        let dir = self.home.strategy_dir(&s.id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("extracted.json"),
            serde_json::to_string_pretty(&e)?,
        )?;
        self.store.set_extracted(&s.id, &e)?;
        let high: Vec<&Risk> = e
            .risks
            .iter()
            .filter(|r| {
                r.severity == Severity::High && (r.code == "no_code" || r.code == "contract")
            })
            .collect();
        if let Some(code) = &e.code {
            std::fs::write(dir.join("strategy.py"), code)?;
        }
        if !high.is_empty() {
            let reason = high
                .iter()
                .map(|r| r.message.clone())
                .collect::<Vec<_>>()
                .join("; ");
            self.transition(
                &s,
                StrategyStatus::Failed,
                "extractor",
                Some(&reason),
                json!({"extractor": e.extractor}),
            )?;
        } else {
            self.transition(&s, StrategyStatus::Extracted, "extractor", None,
                json!({"extractor": e.extractor, "params": e.parameters, "risks": e.risks.iter().map(|r| &r.code).collect::<Vec<_>>()}))?;
        }
        self.store.get_strategy(&s.id)
    }

    // ---------------- market data ----------------
    pub async fn fetch_data(
        &self,
        src: &dyn MarketDataSource,
        symbol: &str,
        count: usize,
    ) -> Result<market::DataMeta> {
        match src.daily_bars(symbol, count).await {
            Ok((bars, meta)) => {
                anyhow::ensure!(
                    src.name() != "sample" || symbol.starts_with("SAMPLE_"),
                    "sample source only serves SAMPLE_* symbols"
                );
                let bars = self.fill_calendar_gaps(bars, &meta);
                market::write_cache(&self.home.data_dir(), &bars, &meta)?;
                let meta = market::read_meta(&self.home.data_dir(), symbol)?;
                self.attest_fetch(symbol, &meta, &bars)?;
                self.mirror_bars(symbol, &meta, &bars);
                Ok(meta)
            }
            Err(e) => {
                self.store.audit(
                    "data",
                    "market_data_fetch",
                    Some(symbol),
                    json!({"source": src.name(), "error": format!("{e:#}")}),
                    false,
                )?;
                Err(e)
            }
        }
    }

    /// Fetch through the multi-source router (priority + fallback + cross-validation).
    pub async fn fetch_routed(
        &self,
        router: &crate::data::router::Router,
        symbol: &str,
        count: usize,
    ) -> Result<market::DataMeta> {
        match router.fetch(symbol, count).await {
            Ok((bars, meta)) => {
                let bars = self.fill_calendar_gaps(bars, &meta);
                market::write_cache(&self.home.data_dir(), &bars, &meta)?;
                let meta = market::read_meta(&self.home.data_dir(), symbol)?;
                self.attest_fetch(symbol, &meta, &bars)?;
                self.mirror_bars(symbol, &meta, &bars);
                Ok(meta)
            }
            Err(e) => {
                self.store.audit(
                    "data",
                    "market_data_fetch",
                    Some(symbol),
                    json!({"router": true, "error": format!("{e:#}")}),
                    false,
                )?;
                Err(e)
            }
        }
    }

    /// Insert volume-0 "suspended" bars for exchange-open days missing from a
    /// feed (needs `data/_calendar_SSE.txt`). Tushare already does this itself.
    fn fill_calendar_gaps(&self, bars: Vec<Bar>, meta: &market::DataMeta) -> Vec<Bar> {
        if meta.source == "sample" {
            return bars;
        }
        // futures: the exchange's own calendar (falls back to SSE, which matches on all but rare days)
        let ex_cal = crate::futures::spec_for(&meta.symbol).and_then(|s| {
            crate::calendars::ExchangeCalendar::load(&self.home.data_dir(), &s.exchange).cal
        });
        let Some(cal) = ex_cal.or_else(|| {
            crate::replay::Calendar::load(&self.home.data_dir().join("_calendar_SSE.txt")).ok()
        }) else {
            return bars;
        };
        let (missing, _) = cal.check(&bars);
        if missing.is_empty() {
            return bars;
        }
        let miss: std::collections::BTreeSet<String> = missing.into_iter().collect();
        let mut out: Vec<Bar> = Vec::with_capacity(bars.len() + miss.len());
        let mut it = bars.into_iter().peekable();
        let first = it.peek().map(|b| b.date.clone()).unwrap_or_default();
        let days: Vec<String> = cal.open.range(first..).cloned().collect();
        for d in days {
            match it.peek() {
                Some(b) if b.date == d => out.push(it.next().unwrap()),
                Some(_) if miss.contains(&d) => {
                    let p = out.last().cloned().expect("first bar present");
                    out.push(Bar {
                        date: d,
                        open: p.close,
                        high: p.close,
                        low: p.close,
                        close: p.close,
                        volume: 0.0,
                        adj_factor: p.adj_factor,
                    });
                }
                Some(_) => {}
                None => break,
            }
        }
        out.extend(it);
        self.log(&format!(
            "filled {} suspended (calendar-gap) days as volume-0 bars",
            miss.len()
        ));
        out
    }

    /// Copy the verified snapshot into the `bars` table (TimescaleDB hypertable on
    /// PostgreSQL). Best effort: the trusted copy remains the hashed snapshot file.
    fn mirror_bars(&self, symbol: &str, meta: &market::DataMeta, bars: &[Bar]) {
        if let Some(sha) = meta.extra.get("snapshot_sha256").and_then(|v| v.as_str()) {
            match self.store.insert_bars(symbol, sha, bars) {
                Ok(n) => self.log(&format!(
                    "{symbol}: {n} bars mirrored to {} (snapshot {})",
                    self.store.backend(),
                    &sha[..12]
                )),
                Err(e) => self.log(&format!("{symbol}: bar mirror failed: {e:#}")),
            }
        }
    }

    /// Write the HMAC-chained attestation that a backtest later trusts: snapshot
    /// sha, source, adjusted, cross-validation verdict, trading-rule facts.
    fn attest_fetch(&self, symbol: &str, meta: &market::DataMeta, bars: &[Bar]) -> Result<()> {
        let validated = crate::data::router::validation_status(meta);
        let rules = scorer::Rules {
            st: meta
                .extra
                .get("st_periods")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
            list_date: meta
                .extra
                .get("list_date")
                .and_then(|v| v.as_str())
                .map(String::from),
        };
        self.store.audit("data", "market_data_fetch", Some(symbol), json!({
            "source": meta.source, "label": meta.label, "snapshot_sha256": meta.extra["snapshot_sha256"], "bars": meta.bars, "start": meta.start, "end": meta.end,
            "adjustment": meta.adjustment, "adjusted": market::is_adjusted(meta, bars), "validation_ok": validated.0, "validation_detail": validated.1,
            "validation": meta.extra.get("validation"), "router": meta.extra.get("router"), "rules": rules, "fetched_at": meta.fetched_at}), true)?;
        Ok(())
    }

    /// Load a dataset for scoring. Trust comes ONLY from the HMAC-verified fetch
    /// attestation: the snapshot sha is mandatory, the bars are read from the
    /// immutable snapshot file, which must be read-only and hash-match.
    pub async fn load_dataset(
        &self,
        symbol: &str,
    ) -> Result<(Vec<Bar>, market::DataMeta, serde_json::Value)> {
        let fail = |m: String| anyhow::Error::from(CoreError::DataIntegrity(m));
        let meta = market::read_meta(&self.home.data_dir(), symbol)
            .map_err(|_| CoreError::NoData(symbol.into()))?;
        let att = self
            .store
            .latest_fetch_attestation(symbol)?
            .ok_or_else(|| {
                fail(format!(
                    "no audited fetch for {symbol}; fetch it with `cmsq data fetch`"
                ))
            })?;
        let sha = att.details["snapshot_sha256"]
            .as_str()
            .filter(|s| s.len() == 64)
            .ok_or_else(|| fail(format!("audited fetch for {symbol} has no snapshot sha")))?
            .to_string();
        if meta.extra.get("snapshot_sha256").and_then(|v| v.as_str()) != Some(sha.as_str()) {
            return Err(fail(format!(
                "{symbol}.meta.json does not match the audited snapshot {}",
                &sha[..12]
            )));
        }
        let snap = self
            .home
            .data_dir()
            .join("snapshots")
            .join(format!("{sha}.csv"));
        let md = std::fs::symlink_metadata(&snap)
            .map_err(|_| fail(format!("snapshot {} missing", snap.display())))?;
        {
            use std::os::unix::fs::PermissionsExt;
            if !md.file_type().is_file() || md.permissions().mode() & 0o222 != 0 {
                return Err(fail(format!(
                    "snapshot {} is not a read-only regular file",
                    snap.display()
                )));
            }
        }
        let text = std::fs::read_to_string(&snap)?;
        let got = hex::encode(sha2::Sha256::digest(text.as_bytes()));
        if got != sha {
            return Err(fail(format!(
                "snapshot content hash {} != audited {}",
                &got[..12],
                &sha[..12]
            )));
        }
        let bars = market::parse_csv(&text)?;
        if att.details["source"] == "sample" {
            let (sb, _) = market::SampleSource
                .daily_bars(symbol, usize::MAX)
                .await
                .map_err(|_| fail("source=sample but not a bundled sample".into()))?;
            if hex::encode(sha2::Sha256::digest(market::to_csv(&sb, true).as_bytes())) != sha {
                return Err(fail(
                    "source=sample but bars differ from the bundled sample".into(),
                ));
            }
        }
        Ok((bars, meta, att.details))
    }

    /// Expected runner image id, written by scripts/build_runner.sh.
    pub fn expected_runner_id(&self) -> Option<String> {
        let read = |p: PathBuf| {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| s.starts_with("sha256:"))
        };
        std::env::var("CMSQ_RUNNER_IMAGE_ID")
            .ok()
            .filter(|s| s.starts_with("sha256:"))
            .or_else(|| read(self.home.secrets.join("runner_image_id")))
            .or_else(|| {
                read(
                    PathBuf::from(std::env::var("HOME").unwrap_or_default())
                        .join(".config/cmsq/runner_image_id"),
                )
            })
    }

    pub fn data_router(
        &self,
        list: Option<&str>,
        validate: bool,
    ) -> (crate::data::router::Router, Vec<String>) {
        let (sources, notes) = crate::data::build_sources(list, &self.home.secrets);
        (
            crate::data::router::Router {
                sources,
                health_file: self.home.data_dir().join("_sources_health.json"),
                validate,
            },
            notes,
        )
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
        self.store.get_strategy(id).map_err(|e| {
            if e.to_string().starts_with("strategy not found") {
                CoreError::NotFound(id.into()).into()
            } else {
                e
            }
        })
    }

    /// The exact text a human must type to promote (never pre-filled by any UI).
    pub fn confirmation_phrase(s: &Strategy) -> String {
        format!("PROMOTE {}", &s.id[..8])
    }

    // ---------------- sandbox: one variant per fresh container ----------------
    #[allow(clippy::too_many_arguments)]
    async fn run_variant(
        &self,
        exec: &dyn SandboxExecutor,
        jd: &Path,
        idx: usize,
        v: &Variant,
        code: &str,
        params: &serde_json::Value,
        side: (f64, f64),
    ) -> std::result::Result<(Vec<f64>, u64), (String, String)> {
        let (lo, tick) = side;
        let io = |e: std::io::Error| ("error".to_string(), format!("host io: {e}"));
        let vd = jd.join("variants").join(format!("{idx:02}"));
        let (vin, vjob, vout) = (vd.join("input"), vd.join("job"), vd.join("out"));
        for d in [&vin, &vjob, &vout] {
            std::fs::create_dir_all(d).map_err(io)?;
        }
        std::fs::write(vin.join("bars.csv"), market::to_csv(&v.bars, false)).map_err(io)?;
        std::fs::write(vjob.join("strategy.py"), code).map_err(io)?;
        std::fs::write(
            vjob.join("job.json"),
            json!({"params": params, "position_min": lo, "pricetick": tick}).to_string(),
        )
        .map_err(io)?;
        // no real timestamps reach the sandbox: pin input mtimes to the fake epoch
        let epoch = std::time::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800); // 2000-01-01
        for f in [
            vin.join("bars.csv"),
            vjob.join("strategy.py"),
            vjob.join("job.json"),
        ] {
            let _ = std::fs::File::options().write(true).open(&f).and_then(|h| {
                h.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(epoch)
                        .set_modified(epoch),
                )
            });
        }
        let abs = |p: &Path| std::fs::canonicalize(p).map_err(io);
        let mut mounts = vec![
            Mount {
                host: abs(&vin)?,
                container: "/input".into(),
                read_only: true,
            },
            Mount {
                host: abs(&vjob)?,
                container: "/job".into(),
                read_only: true,
            },
            Mount {
                host: abs(&vout)?,
                container: "/out".into(),
                read_only: false,
            },
        ];
        if !exec.hardened() {
            mounts.push(Mount {
                host: abs(&self.home.runner_dir())?,
                container: "/runner".into(),
                read_only: true,
            });
        }
        let req = SandboxRequest {
            id: format!("{}-{idx:02}", jd.file_name().unwrap().to_string_lossy()),
            image: RUNNER_IMAGE.into(),
            command: RUNNER_CMD.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            mounts,
            limits: self.limits.clone(),
        };
        let permit = self.slots.acquire().await.map_err(|_| {
            (
                "error".to_string(),
                "container slot pool closed".to_string(),
            )
        })?;
        let r = exec
            .execute(&req)
            .await
            .map_err(|e| ("error".to_string(), format!("sandbox error: {e}")));
        drop(permit);
        let r = r?;
        let _ = std::fs::write(vd.join("stdout.log"), &r.stdout);
        let _ = std::fs::write(vd.join("stderr.log"), &r.stderr);
        let _ = std::fs::write(vd.join("policy.json"), r.policy.to_string());
        let last = || {
            r.stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .chars()
                .take(300)
                .collect::<String>()
        };
        let pids = [
            "Resource temporarily unavailable",
            "can't start new thread",
            "BlockingIOError",
        ];
        let pid_hit = pids.iter().any(|p| r.stderr.contains(p))
            || r.policy["pids_hit"].as_bool().unwrap_or(false)
            || (r.outcome == Outcome::OomKilled && code.contains("fork"));
        match r.outcome {
            Outcome::OomKilled if pid_hit => {
                return Err((
                    "pids_exhausted".into(),
                    format!(
                        "process limit ({} pids) exhausted, then out of memory",
                        self.limits.pids
                    ),
                ))
            }
            Outcome::TimedOut => {
                return Err((
                    "timeout".into(),
                    format!(
                        "sandbox timeout after {}s (variant {})",
                        self.limits.timeout_secs, v.name
                    ),
                ))
            }
            Outcome::OomKilled => {
                return Err((
                    "oom".into(),
                    format!("out of memory (limit {} MB)", self.limits.memory_mb),
                ))
            }
            Outcome::Exited => {}
        }
        if r.stdout.len() >= OUTPUT_CAP || r.stderr.len() >= OUTPUT_CAP {
            return Err((
                "output_cap".into(),
                format!(
                    "output exceeded {} MiB cap; job killed from scoring",
                    OUTPUT_CAP >> 20
                ),
            ));
        }
        if r.exit_code != 0 {
            if pid_hit {
                return Err((
                    "pids_exhausted".into(),
                    format!(
                        "process limit ({} pids) exhausted: {}",
                        self.limits.pids,
                        last()
                    ),
                ));
            }
            return Err((
                "strategy_error".into(),
                format!("runner exit {}: {}", r.exit_code, last()),
            ));
        }
        // untrusted output: regular file only (no symlink), size-capped, strict parse
        let p = vout.join("positions.csv");
        let md = std::fs::symlink_metadata(&p)
            .map_err(|_| ("bad_output".to_string(), "no positions.csv".to_string()))?;
        if !md.file_type().is_file() || md.len() > 8 * 1024 * 1024 {
            return Err((
                "bad_output".into(),
                "positions.csv is not a regular file under 8 MiB".into(),
            ));
        }
        let text = {
            use std::io::Read;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc_o_nofollow())
                .open(&p)
                .map_err(io)?;
            let mut s = String::new();
            f.by_ref()
                .take(8 * 1024 * 1024)
                .read_to_string(&mut s)
                .map_err(|_| {
                    (
                        "bad_output".to_string(),
                        "positions.csv is not UTF-8".to_string(),
                    )
                })?;
            s
        };
        let dates: Vec<String> = v.bars.iter().map(|b| b.date.clone()).collect();
        let pos = probe::parse_positions_range(&text, &dates, lo)
            .map_err(|e| ("bad_output".to_string(), e))?;
        Ok((pos, r.duration_ms))
    }

    /// Runs a shuffled probe plan, one fresh container per variant.
    async fn run_plan(
        &self,
        exec: &dyn SandboxExecutor,
        jd: &Path,
        plan: &[Variant],
        code: &str,
        params: &serde_json::Value,
        side: (f64, f64),
    ) -> std::result::Result<(Vec<f64>, ProbeReport, u64), (String, String)> {
        use futures_util::stream::{self, StreamExt};
        let n = plan.len();
        type VariantOut = (
            usize,
            std::result::Result<(Vec<f64>, u64), (String, String)>,
        );
        let outs: Vec<VariantOut> = stream::iter(plan.iter().enumerate())
            .map(|(i, v)| async move {
                self.log(&format!(
                    "container {}/{n}: variant {} ({} bars)",
                    i + 1,
                    v.name,
                    v.bars.len()
                ));
                (
                    i,
                    self.run_variant(exec, jd, i, v, code, params, side).await,
                )
            })
            .buffer_unordered(self.parallel.max(1))
            .collect()
            .await;
        let mut results: Vec<(usize, Vec<f64>)> = vec![];
        let mut total_ms = 0;
        let mut outs = outs;
        outs.sort_by_key(|o| o.0);
        for (i, r) in outs {
            let (pos, ms) = r?;
            total_ms += ms;
            results.push((i, pos));
        }
        let fi = plan.iter().position(|v| v.name == "full").ok_or((
            "error".to_string(),
            "probe plan has no full variant".to_string(),
        ))?;
        let full = results
            .iter()
            .find(|r| r.0 == fi)
            .ok_or((
                "error".to_string(),
                "full variant produced no result".to_string(),
            ))?
            .1
            .clone();
        let dates: Vec<String> = plan[fi].bars.iter().map(|b| b.date.clone()).collect();
        let mismatches: Vec<serde_json::Value> = results
            .iter()
            .filter(|r| r.0 != fi)
            .filter_map(|(i, p)| probe::compare(&full, &plan[*i], p, &dates))
            .collect();
        let rep = ProbeReport {
            detected: !mismatches.is_empty(),
            variants: plan.len(),
            containers: results.len(),
            order: plan.iter().map(|v| v.name.clone()).collect(),
            mismatches,
        };
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

    fn write_equity(
        path: &Path,
        bars: &[Bar],
        pos: &[f64],
        sim: &scorer::SimResult,
        sim2: &scorer::SimResult,
    ) -> Result<()> {
        let mut s = String::from("date,equity,equity_cost2x,target_position,exposure\n");
        for i in 0..bars.len() {
            s.push_str(&format!(
                "{},{:.2},{:.2},{:.6},{:.6}\n",
                bars[i].date, sim.equity[i], sim2.equity[i], pos[i], sim.exposure[i]
            ));
        }
        std::fs::write(path, s)?;
        Ok(())
    }

    // ---------------- one scored development run (shared by backtest / sweep / portfolio) ----------------
    /// Sandbox + trusted scoring + atomic trial-ledger insert for one
    /// (strategy, symbol, params). Never changes the strategy's state.
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate_once(
        &self,
        s: &Strategy,
        e: &ExtractedStrategy,
        code: &str,
        ds: &Dataset,
        params: &serde_json::Value,
        exec: &dyn SandboxExecutor,
        run_kind: &str,
        group: Option<&str>,
    ) -> Result<RunOutcome> {
        let (bars, meta, att) = (&ds.bars, &ds.meta, &ds.att);
        let symbol = ds.symbol.as_str();
        anyhow::ensure!(
            bars.len() >= 300,
            "need at least 300 bars, {symbol} has {}",
            bars.len()
        );
        let dataset_sha = att["snapshot_sha256"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let rules: scorer::Rules = serde_json::from_value(att["rules"].clone()).unwrap_or_default();
        let fut = crate::futures::spec_for(symbol);
        let dev_end = ((bars.len() as f64) * self.dev_frac) as usize;
        let dev = &bars[..dev_end];
        let identity = exec.runner_identity(RUNNER_IMAGE).await;
        let job_id = uuid::Uuid::new_v4().to_string();
        let jd = self.home.job_dir(&s.id, &job_id);
        std::fs::create_dir_all(&jd)?;
        let job = JobRecord {
            id: job_id.clone(),
            strategy_id: s.id.clone(),
            backend: exec.name().into(),
            image: if exec.hardened() {
                RUNNER_IMAGE.into()
            } else {
                "host-python (dev)".into()
            },
            symbol: symbol.into(),
            data_source: meta.source.clone(),
            status: "running".into(),
            exit_code: None,
            started_at: Utc::now(),
            finished_at: None,
            duration_ms: None,
            job_dir: jd.display().to_string(),
            error: None,
            result: None,
            owner_pid: None,
            runner_identity: identity.clone(),
            kind: if run_kind == "single" {
                "dev".into()
            } else {
                run_kind.into()
            },
            owner_host: None,
        };
        self.store.insert_job(&job)?;
        let seed = Rng::from_uuid().next_u64();
        let mut rng = Rng::new(seed);
        let view = probe::container_view(dev);
        // probe the whole SCORED domain, from the first scored bar (issue #6)
        let score_from = ((dev_end as f64) * self.warmup_frac) as usize;
        let (rcuts, tcuts) = probe::choose_cuts(dev_end, score_from, &self.probe, &mut rng);
        let mcuts = probe::strata_cuts(dev_end, score_from, &self.probe, &mut rng);
        let trunc: Vec<usize> = rcuts.iter().chain(&tcuts).copied().collect();
        let max_move = match &fut {
            Some(f) => f.limit_pct - 0.005,
            None => scorer::limit_pct(symbol, &dev[0].date) - 0.005,
        };
        let plan = probe::plan_with_mirrors(
            &view,
            &trunc,
            &rcuts,
            &mcuts,
            self.probe.resamples,
            max_move,
            &mut rng,
        );
        self.store.audit("sandbox", "sandbox_start", Some(&s.id), json!({"job": job_id, "backend": exec.name(), "hardened": exec.hardened(),
            "runner_identity": identity, "limits": self.limits, "symbol": symbol, "data_source": meta.source, "run_kind": run_kind, "group": group, "params": params,
            "dev_window": {"start": dev[0].date, "end": dev[dev_end - 1].date, "bars": dev_end}, "vault_bars": bars.len() - dev_end,
            "dataset_sha256": dataset_sha, "probe": {"seed": seed.to_string(), "random_cuts": rcuts, "tail_cuts": tcuts, "mirror_cuts": mcuts, "score_from": score_from, "config": self.probe, "container_dates": "fake weekday sequence from 2000-01-03"},
            "variants": plan.iter().map(|v| &v.name).collect::<Vec<_>>()}), true)?;
        self.log(&format!(
            "job {} [{run_kind}] {symbol}: {} variants, one fresh container each, random order",
            &job_id[..8],
            plan.len()
        ));
        let t0 = std::time::Instant::now();
        let lo = if fut.is_some() { -1.0 } else { 0.0 };
        let (full, probe_rep, _) = match self
            .run_plan(
                exec,
                &jd,
                &plan,
                code,
                params,
                (lo, fut.as_ref().map(|f| f.tick).unwrap_or(0.01)),
            )
            .await
        {
            Ok(x) => x,
            Err((status, reason)) => {
                let ms = t0.elapsed().as_millis() as i64;
                self.store
                    .finish_job(&job_id, &status, None, ms, Some(&reason), None)?;
                self.store.audit(
                    "sandbox",
                    "sandbox_finish",
                    Some(&s.id),
                    json!({"job": job_id, "status": status, "error": reason}),
                    false,
                )?;
                self.log(&format!("FAILED ({status}): {reason}"));
                return Ok(RunOutcome::Failed {
                    job_id,
                    status,
                    reason,
                });
            }
        };
        let ms = t0.elapsed().as_millis() as i64;

        // ---- trusted scoring ----
        let (sim, sim2, bh) = match &fut {
            Some(spec) => {
                let fx = crate::futures::FuturesCosts::default();
                (
                    crate::futures::simulate(spec, dev, &full, &fx, &self.costs),
                    crate::futures::simulate(spec, dev, &full, &fx, &self.costs.stressed(2.0)),
                    crate::futures::simulate(spec, dev, &vec![1.0; dev_end], &fx, &self.costs),
                )
            }
            None => (
                scorer::simulate_with(symbol, dev, &full, &self.costs, &rules),
                scorer::simulate_with(symbol, dev, &full, &self.costs.stressed(2.0), &rules),
                scorer::simulate_with(symbol, dev, &vec![1.0; dev_end], &self.costs, &rules),
            ),
        };
        let warm = ((dev_end as f64) * self.warmup_frac) as usize;
        let dm = scorer::metrics(dev, &sim, warm, dev_end);
        let dm2 = scorer::metrics(dev, &sim2, warm, dev_end);
        let fold_len = (dev_end - warm) / self.folds.max(1);
        let folds: Vec<Metrics> = (0..self.folds)
            .map(|k| {
                scorer::metrics(
                    dev,
                    &sim,
                    warm + k * fold_len,
                    if k + 1 == self.folds {
                        dev_end
                    } else {
                        warm + (k + 1) * fold_len
                    },
                )
            })
            .collect();
        let bhm = scorer::metrics(dev, &bh, warm, dev_end);
        Self::write_equity(&jd.join("equity.csv"), dev, &full, &sim, &sim2)?;

        // ---- library-wide trial ledger + DSR (insert and snapshot are one locked transaction) ----
        let ikey = crate::extract::idea_key(code);
        let phash = params_hash(params);
        let all = self.store.record_trial(&NewTrial {
            strategy_id: &s.id,
            job_id: &job_id,
            idea_key: &ikey,
            params_hash: &phash,
            symbol,
            dataset_sha: &dataset_sha,
            bars: dm.bars,
            sharpe_daily: dm.sharpe_daily,
            positions: &full,
            lookahead: probe_rep.detected,
            run_kind,
            group_id: group,
        })?;
        let me = all.last().map(|t| t.id).unwrap_or(0);
        let near_dups: Vec<String> = all
            .iter()
            .filter(|t| {
                t.id != me
                    && t.strategy_id != s.id
                    && t.symbol == symbol
                    && t.positions.len() == full.len()
            })
            .filter(|t| t.idea_key == ikey || corr(&t.positions, &full).is_some_and(|c| c > 0.9))
            .map(|t| t.strategy_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let (dsr, trial_info) = self.dsr_for(&all, &dm);
        let adjusted = att["adjusted"].as_bool().unwrap_or(false) || fut.is_some();
        let validated = (
            att["validation_ok"].as_bool().unwrap_or(false),
            att["validation_detail"]
                .as_str()
                .unwrap_or("no attested validation")
                .to_string(),
        );
        let mut trial = trial_info;
        trial["idea_key"] = json!(ikey);
        trial["params_hash"] = json!(phash);
        trial["ledger_position"] = json!(all.len());
        trial["near_duplicates_of"] = json!(near_dups);
        let synthetic = !attested_real_data(att, symbol);
        let capacity = scorer::capacity(
            dev,
            &sim,
            fut.as_ref().map(|f| f.multiplier).unwrap_or(1.0),
            warm,
            dev_end,
            0.05,
        );
        let result = json!({
            "schema_version": 3, "synthetic_data": synthetic, "data_class_source": att["source"], "capacity_cny": capacity, "computed_by": "cmsq-core scorer (host, trusted)", "symbol": symbol, "execution": if fut.is_some() { crate::futures::EXECUTION_LABEL } else { scorer::EXECUTION_LABEL },
            "market": if fut.is_some() { "cn_futures" } else { "cn_ashare" }, "futures_spec": fut,
            "run_kind": run_kind, "group_id": group, "params": params,
            "costs": self.costs, "initial_capital": scorer::INITIAL_CAPITAL, "runner_identity": identity, "backend": exec.name(), "hardened": exec.hardened(),
            "data_provenance": meta, "data_attestation": att, "rules": rules, "dataset_sha256": dataset_sha, "data_adjusted": adjusted, "data_cross_validation": {"ok": validated.0, "detail": validated.1},
            "split": {"development": {"start": dev[0].date, "end": dev[dev_end - 1].date, "bars": dev_end, "warmup_bars": warm},
                      "vault": {"start": bars[dev_end].date, "end": bars.last().unwrap().date, "bars": bars.len() - dev_end, "note": "never shown to the strategy before promote"}},
            "development": dm, "development_cost_2x": dm2, "walk_forward_folds": folds, "buy_and_hold": bhm,
            "trades": sim.trades, "blocked": {"limit_up": sim.blocked_limit_up, "limit_down": sim.blocked_limit_down, "suspended": sim.blocked_suspended},
            "costs_paid": sim.costs_paid, "close_today": {"lots": sim.close_today_lots, "fees": sim.close_today_fees},
            "trade_stats": crate::tradestats::stats_window(&sim.fills, fut.as_ref().map(|f| f.multiplier).unwrap_or(1.0), warm),
            "lookahead_probe": probe_rep, "trial": trial,
            "dsr": dsr, "latest_position": {"date": dev[dev_end - 1].date, "target_position": full[dev_end - 1]}, "equity_csv": jd.join("equity.csv").display().to_string(),
        });
        std::fs::write(
            jd.join("result.json"),
            serde_json::to_string_pretty(&result)?,
        )?;
        self.store
            .finish_job(&job_id, "succeeded", Some(0), ms, None, Some(&result))?;
        self.store.audit("sandbox", "sandbox_finish", Some(&s.id), json!({"job": job_id, "status": "succeeded", "ms": ms, "containers": probe_rep.containers, "run_kind": run_kind}), true)?;
        let high: Vec<String> = e
            .risks
            .iter()
            .filter(|r| {
                r.severity == Severity::High
                    && crate::extract::BLOCKING_RISKS.contains(&r.code.as_str())
            })
            .map(|r| r.code.clone())
            .collect();
        Ok(RunOutcome::Scored(Box::new(Scored {
            job_id,
            symbol: symbol.into(),
            params: params.clone(),
            dm,
            dm2,
            folds,
            bhm,
            probe: probe_rep,
            adjusted,
            validated,
            identity,
            hardened: exec.hardened(),
            dsr,
            positions: full,
            dev_bars: dev.to_vec(),
            result,
            high_risks: high,
            equity: sim.equity,
            synthetic,
            capacity,
        })))
    }

    /// DSR against the whole ledger. N = distinct (idea, parameter set)
    /// configurations — every sweep point is its own trial — floored at
    /// `dsr_min_trials`; variance from probe-clean trials only (robust).
    pub fn dsr_for(
        &self,
        all: &[crate::store::TrialRow],
        dm: &Metrics,
    ) -> (scorer::Dsr, serde_json::Value) {
        let pool: Vec<f64> = all
            .iter()
            .filter(|t| !t.lookahead)
            .map(|t| t.sharpe_daily)
            .collect();
        let (var, kept, implausible) = scorer::robust_sr_variance(&pool);
        let configs = all
            .iter()
            .map(|t| (t.idea_key.as_str(), t.params_hash.as_str()))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let selections = all
            .iter()
            .map(|t| t.selection_key())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let clusters = all
            .iter()
            .map(|t| t.idea_key.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let n_eff = selections.max(self.gate.dsr_min_trials);
        let dsr = scorer::deflated_sharpe_v(dm, var, n_eff);
        (
            dsr,
            json!({"library_trials": all.len(), "idea_clusters": clusters, "configurations": configs, "selections": selections,
            "counting_rule": "N = distinct (idea, params, symbol, snapshot, scoring bars, run kind, group); exact reruns dedupe; no correlation discount (conservative)", "n_effective": n_eff, "n_floor": self.gate.dsr_min_trials,
            "variance_pool": kept, "excluded_lookahead": all.len() - pool.len(), "excluded_implausible": implausible}),
        )
    }

    fn gate_scored(&self, sc: &Scored) -> gate::GateOutcome {
        gate::evaluate(
            &gate::GateInput {
                job_status: "succeeded",
                hardened: sc.hardened,
                runner_identity: sc.identity.as_deref(),
                dev: &sc.dm,
                dev_cost_2x: &sc.dm2,
                folds: &sc.folds,
                dsr: &sc.dsr,
                probe: &sc.probe,
                data_adjusted: sc.adjusted,
                data_validated: sc.validated.clone(),
                high_risks: &sc.high_risks,
                buy_hold: &sc.bhm,
                expected_runner: self.expected_runner_id().as_deref(),
                synthetic: sc.synthetic,
                capacity: sc.capacity,
            },
            &self.gate,
        )
    }

    fn code_of(s: &Strategy) -> Result<(ExtractedStrategy, String)> {
        let e = s
            .extracted
            .clone()
            .ok_or_else(|| CoreError::NoCode(s.id.clone()))?;
        let code = e
            .code
            .clone()
            .ok_or_else(|| CoreError::NoCode(s.id.clone()))?;
        Ok((e, code))
    }

    async fn dataset_or_refuse(&self, s: &Strategy, symbol: &str) -> Result<Dataset> {
        match self.load_dataset(symbol).await {
            Ok((bars, meta, att)) => Ok(Dataset {
                symbol: symbol.into(),
                bars,
                meta,
                att,
            }),
            Err(err) => {
                self.store.audit(
                    "orchestrator",
                    "data_integrity_refused",
                    Some(&s.id),
                    json!({"symbol": symbol, "error": format!("{err:#}")}),
                    false,
                )?;
                Err(err)
            }
        }
    }

    /// Gate the chosen run, write the evaluation, move Evaluated -> GatePassed|Rejected.
    fn conclude(
        &self,
        s: &Strategy,
        sc: &Scored,
        extra: serde_json::Value,
    ) -> Result<(StrategyStatus, Vec<String>)> {
        let s = self.get(&s.id)?;
        self.transition(
            &s,
            StrategyStatus::Evaluated,
            "orchestrator",
            None,
            json!({"job": sc.job_id}),
        )?;
        let g = self.gate_scored(sc);
        let ev = Evaluation {
            id: uuid::Uuid::new_v4().to_string(),
            strategy_id: s.id.clone(),
            job_id: sc.job_id.clone(),
            passed: g.passed,
            reasons: g.reasons.clone(),
            checks: g.checks.clone(),
            created_at: Utc::now(),
        };
        self.store.insert_evaluation(&ev)?;
        self.store.audit("gate", "evaluate", Some(&s.id), json!({"job": sc.job_id, "passed": g.passed, "reasons": g.reasons, "dsr": sc.dsr.dsr, "trials": sc.dsr.trials, "extra": extra}), true)?;
        let s = self.get(&s.id)?;
        let st = if g.passed {
            self.transition(
                &s,
                StrategyStatus::GatePassed,
                "gate",
                Some("gate passed on development data — vault holdout runs at manual promote"),
                json!({"job": sc.job_id}),
            )?;
            StrategyStatus::GatePassed
        } else {
            self.transition(
                &s,
                StrategyStatus::Rejected,
                "gate",
                Some(&g.reasons.join("; ")),
                json!({"job": sc.job_id}),
            )?;
            StrategyStatus::Rejected
        };
        self.log(&format!(
            "{st}: Sharpe {:.2}, DSR {:.3} (N={}), {}/{} folds positive",
            sc.dm.sharpe,
            sc.dsr.dsr,
            sc.dsr.trials,
            sc.folds.iter().filter(|f| f.total_return > 0.0).count(),
            sc.folds.len()
        ));
        Ok((st, g.reasons))
    }

    // ---------------- backtest (development window) ----------------
    pub async fn backtest(
        &self,
        id: &str,
        symbol: &str,
        exec: &dyn SandboxExecutor,
        params_override: Option<serde_json::Value>,
    ) -> Result<BacktestSummary> {
        let s = self.get(id)?;
        if let Err(e) = state::check(s.status, StrategyStatus::Backtesting) {
            self.store.audit(
                "orchestrator",
                "transition_denied",
                Some(&s.id),
                json!({"from": s.status, "to": "Backtesting", "error": e.to_string()}),
                false,
            )?;
            return Err(e);
        }
        let (e, code) = Self::code_of(&s)?;
        let ds = self.dataset_or_refuse(&s, symbol).await?;
        anyhow::ensure!(
            ds.bars.len() >= 300,
            "need at least 300 bars, {symbol} has {}",
            ds.bars.len()
        );
        self.transition(
            &s,
            StrategyStatus::Backtesting,
            "orchestrator",
            None,
            json!({"symbol": symbol, "backend": exec.name()}),
        )?;
        self.store.bump_attempts(&s.id)?;
        let params = Self::params_for(&e, params_override);
        let out = self
            .evaluate_once(&s, &e, &code, &ds, &params, exec, "single", None)
            .await;
        let s = self.get(&s.id)?;
        match out {
            Err(err) => {
                let _ = self.transition(
                    &s,
                    StrategyStatus::Failed,
                    "orchestrator",
                    Some(&format!("{err:#}")),
                    json!({}),
                );
                Err(err)
            }
            Ok(RunOutcome::Failed {
                job_id,
                status,
                reason,
            }) => {
                self.transition(
                    &s,
                    StrategyStatus::Failed,
                    "orchestrator",
                    Some(&reason),
                    json!({"job": job_id, "status": status}),
                )?;
                Ok(BacktestSummary {
                    strategy_id: s.id.clone(),
                    job_id,
                    status: StrategyStatus::Failed,
                    job_status: status,
                    reasons: vec![reason],
                    oos: None,
                })
            }
            Ok(RunOutcome::Scored(sc)) => {
                let (st, reasons) = self.conclude(&s, &sc, json!({}))?;
                Ok(BacktestSummary {
                    strategy_id: s.id.clone(),
                    job_id: sc.job_id.clone(),
                    status: st,
                    job_status: "succeeded".into(),
                    reasons,
                    oos: Some(json!(sc.dm)),
                })
            }
        }
    }

    /// Run several (strategy, symbol) backtests concurrently. Runs of the same
    /// strategy stay sequential (its state machine is single-track); different
    /// strategies proceed in parallel, bounded by `workers` and by the global
    /// container slot pool (Docker capacity).
    pub async fn backtest_batch(
        &self,
        pairs: &[(String, String)],
        exec: &dyn SandboxExecutor,
        workers: usize,
    ) -> Vec<(String, String, Result<BacktestSummary>)> {
        use futures_util::stream::{self, StreamExt};
        let mut groups: Vec<(String, Vec<String>)> = vec![];
        for (id, sym) in pairs {
            match groups.iter_mut().find(|g| &g.0 == id) {
                Some(g) => g.1.push(sym.clone()),
                None => groups.push((id.clone(), vec![sym.clone()])),
            }
        }
        let outs: Vec<Vec<(String, String, Result<BacktestSummary>)>> = stream::iter(groups)
            .map(|(id, syms)| async move {
                let mut v = vec![];
                for sym in syms {
                    let r = self.backtest(&id, &sym, exec, None).await;
                    v.push((id.clone(), sym, r));
                }
                v
            })
            .buffer_unordered(workers.max(1))
            .collect()
            .await;
        outs.into_iter().flatten().collect()
    }

    // ---------------- P1: parameter sweep ----------------
    /// Grid sweep on one symbol. Every point is a full sandboxed, probed run and
    /// its own trial in the library ledger (N grows by the number of distinct
    /// points). The best probe-clean point is then gated with the DSR recomputed
    /// against the ledger AFTER the whole sweep, so picking the max of K points
    /// pays for all K.
    pub async fn sweep(
        &self,
        id: &str,
        symbol: &str,
        grid: &serde_json::Map<String, serde_json::Value>,
        exec: &dyn SandboxExecutor,
        workers: usize,
    ) -> Result<SweepSummary> {
        use futures_util::stream::{self, StreamExt};
        let s = self.get(id)?;
        state::check(s.status, StrategyStatus::Backtesting)?;
        let (e, code) = Self::code_of(&s)?;
        let points = expand_grid(&e, grid)?;
        let ds = self.dataset_or_refuse(&s, symbol).await?;
        anyhow::ensure!(
            ds.bars.len() >= 300,
            "need at least 300 bars, {symbol} has {}",
            ds.bars.len()
        );
        self.transition(
            &s,
            StrategyStatus::Backtesting,
            "orchestrator",
            None,
            json!({"symbol": symbol, "sweep_points": points.len()}),
        )?;
        self.store.bump_attempts(&s.id)?;
        let run_id = uuid::Uuid::new_v4().to_string();
        self.store.insert_run(&crate::store::ResearchRun { id: run_id.clone(), kind: "sweep".into(), strategy_id: Some(s.id.clone()),
            spec: json!({"symbol": symbol, "grid": grid, "points": points.len(), "workers": workers}), result: None, status: "running".into(),
            created_at: Utc::now().to_rfc3339(), finished_at: None })?;
        self.store.audit(
            "orchestrator",
            "sweep_start",
            Some(&s.id),
            json!({"run": run_id, "symbol": symbol, "grid": grid, "points": points.len()}),
            true,
        )?;
        let (ds_ref, e_ref, code_ref, s_ref, run_ref) =
            (&ds, &e, code.as_str(), &s, run_id.as_str());
        let mut outs: Vec<(usize, Result<RunOutcome>)> =
            stream::iter(points.iter().cloned().enumerate())
                .map(|(i, pt)| async move {
                    let params = Self::params_for(e_ref, Some(pt));
                    (
                        i,
                        self.evaluate_once(
                            s_ref,
                            e_ref,
                            code_ref,
                            ds_ref,
                            &params,
                            exec,
                            "sweep",
                            Some(run_ref),
                        )
                        .await,
                    )
                })
                .buffer_unordered(workers.max(1))
                .collect()
                .await;
        outs.sort_by_key(|o| o.0);
        let mut table = vec![];
        let mut best: Option<Box<Scored>> = None;
        for (i, o) in outs {
            match o {
                Ok(RunOutcome::Scored(sc)) => {
                    table.push(json!({"point": i, "params": points[i], "job": sc.job_id, "status": "succeeded", "sharpe": sc.dm.sharpe, "total_return": sc.dm.total_return,
                        "max_drawdown": sc.dm.max_drawdown, "dsr_at_record": sc.dsr.dsr, "lookahead": sc.probe.detected}));
                    if !sc.probe.detected && best.as_ref().is_none_or(|b| sc.dm.sharpe > b.dm.sharpe) {
                        best = Some(sc);
                    }
                }
                Ok(RunOutcome::Failed { job_id, status, reason }) => table.push(json!({"point": i, "params": points[i], "job": job_id, "status": status, "error": reason})),
                Err(err) => table.push(json!({"point": i, "params": points[i], "status": "error", "error": format!("{err:#}")})),
            }
        }
        let s = self.get(&s.id)?;
        let Some(mut best) = best else {
            let r = json!({"points": table, "best": null});
            self.store.finish_run(&run_id, "failed", &r)?;
            self.transition(
                &s,
                StrategyStatus::Failed,
                "orchestrator",
                Some("sweep: no probe-clean point succeeded"),
                json!({"run": run_id}),
            )?;
            return Ok(SweepSummary {
                run_id,
                strategy_id: s.id.clone(),
                status: StrategyStatus::Failed,
                best_job: None,
                best_params: None,
                points: table,
                dsr: None,
                reasons: vec!["no probe-clean point".into()],
            });
        };
        // the selected point is judged against the FINAL ledger (all K points recorded)
        let (dsr, info) = self.dsr_for(&self.store.trials()?, &best.dm);
        best.dsr = dsr.clone();
        let (st, reasons) = self.conclude(
            &s,
            &best,
            json!({"sweep_run": run_id, "points": table.len(), "trial_info": info}),
        )?;
        let r = json!({"points": table, "best": {"job": best.job_id, "params": best.params, "development": best.dm, "dsr": dsr, "trial_info": info}, "status": st, "reasons": reasons});
        self.store.finish_run(&run_id, "succeeded", &r)?;
        Ok(SweepSummary {
            run_id,
            strategy_id: s.id.clone(),
            status: st,
            best_job: Some(best.job_id.clone()),
            best_params: Some(best.params.clone()),
            points: table,
            dsr: Some(dsr),
            reasons,
        })
    }

    // ---------------- P1: multi-symbol portfolio ----------------
    /// Runs the strategy on every symbol (each a sandboxed, probed leg), then
    /// simulates ONE account with shared cash on the common development dates,
    /// allocating capital by `alloc`. The portfolio is recorded as its own trial.
    /// Research only: it never touches the vault and never changes strategy state.
    pub async fn portfolio(
        &self,
        id: &str,
        symbols: &[String],
        alloc: scorer::Alloc,
        params_override: Option<serde_json::Value>,
        exec: &dyn SandboxExecutor,
        workers: usize,
    ) -> Result<PortfolioSummary> {
        use futures_util::stream::{self, StreamExt};
        anyhow::ensure!(symbols.len() >= 2, "a portfolio needs at least 2 symbols");
        let s = self.get(id)?;
        let (e, code) = Self::code_of(&s)?;
        let mut dss = vec![];
        for sym in symbols {
            dss.push(self.dataset_or_refuse(&s, sym).await?);
        }
        let params = Self::params_for(&e, params_override);
        let run_id = uuid::Uuid::new_v4().to_string();
        let spec = json!({"symbols": symbols, "alloc": alloc, "params": params});
        self.store.insert_run(&crate::store::ResearchRun {
            id: run_id.clone(),
            kind: "portfolio".into(),
            strategy_id: Some(s.id.clone()),
            spec: spec.clone(),
            result: None,
            status: "running".into(),
            created_at: Utc::now().to_rfc3339(),
            finished_at: None,
        })?;
        self.store.audit(
            "orchestrator",
            "portfolio_start",
            Some(&s.id),
            json!({"run": run_id, "spec": spec}),
            true,
        )?;
        let (e_ref, code_ref, s_ref, run_ref, p_ref) =
            (&e, code.as_str(), &s, run_id.as_str(), &params);
        let mut legs: Vec<(usize, Result<RunOutcome>)> = stream::iter(dss.iter().enumerate())
            .map(|(i, ds)| async move {
                (
                    i,
                    self.evaluate_once(
                        s_ref,
                        e_ref,
                        code_ref,
                        ds,
                        p_ref,
                        exec,
                        "portfolio_leg",
                        Some(run_ref),
                    )
                    .await,
                )
            })
            .buffer_unordered(workers.max(1))
            .collect()
            .await;
        legs.sort_by_key(|l| l.0);
        let mut scored = vec![];
        let mut problems = vec![];
        for (i, l) in legs {
            match l {
                Ok(RunOutcome::Scored(sc)) if !sc.probe.detected => scored.push(sc),
                Ok(RunOutcome::Scored(sc)) => {
                    problems.push(format!("{}: look-ahead probe failed", sc.symbol))
                }
                Ok(RunOutcome::Failed { status, reason, .. }) => {
                    problems.push(format!("{}: {status}: {reason}", symbols[i]))
                }
                Err(err) => problems.push(format!("{}: {err:#}", symbols[i])),
            }
        }
        if !problems.is_empty() {
            let r = json!({"problems": problems});
            self.store.finish_run(&run_id, "failed", &r)?;
            return Ok(PortfolioSummary {
                run_id,
                strategy_id: s.id.clone(),
                passed: false,
                reasons: problems,
                result: r,
            });
        }
        // common development dates (every leg's dev window, which ends before its vault)
        let mut common: Option<std::collections::BTreeSet<String>> = None;
        for sc in &scored {
            let d: std::collections::BTreeSet<String> =
                sc.dev_bars.iter().map(|b| b.date.clone()).collect();
            common = Some(match common {
                None => d,
                Some(c) => c.intersection(&d).cloned().collect(),
            });
        }
        let dates: Vec<String> = common.unwrap_or_default().into_iter().collect();
        anyhow::ensure!(
            dates.len() >= 250,
            "only {} common development dates across the symbols",
            dates.len()
        );
        let legs_in: Vec<scorer::Leg> = scored
            .iter()
            .map(|sc| {
                let idx: std::collections::HashMap<&str, usize> = sc
                    .dev_bars
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (b.date.as_str(), i))
                    .collect();
                let rows: Vec<usize> = dates.iter().map(|d| idx[d.as_str()]).collect();
                let rules: scorer::Rules = sc.result["rules"]
                    .clone()
                    .pipe(|v| serde_json::from_value(v).unwrap_or_default());
                scorer::Leg {
                    symbol: sc.symbol.clone(),
                    bars: rows.iter().map(|&i| sc.dev_bars[i].clone()).collect(),
                    pos: rows.iter().map(|&i| sc.positions[i]).collect(),
                    rules,
                }
            })
            .collect();
        let ps = scorer::simulate_portfolio(&legs_in, &alloc, &self.costs);
        let ps2 = scorer::simulate_portfolio(&legs_in, &alloc, &self.costs.stressed(2.0));
        let n = dates.len();
        let warm = ((n as f64) * self.warmup_frac) as usize;
        let pb = &legs_in[0].bars; // dates only
        let pm = scorer::metrics(pb, &ps.sim, warm, n);
        let pm2 = scorer::metrics(pb, &ps2.sim, warm, n);
        let fold_len = (n - warm) / self.folds.max(1);
        let folds: Vec<Metrics> = (0..self.folds)
            .map(|k| {
                scorer::metrics(
                    pb,
                    &ps.sim,
                    warm + k * fold_len,
                    if k + 1 == self.folds {
                        n
                    } else {
                        warm + (k + 1) * fold_len
                    },
                )
            })
            .collect();
        let ew_bh = scorer::simulate_portfolio(
            &legs_in
                .iter()
                .map(|l| scorer::Leg {
                    pos: vec![1.0; n],
                    ..l.clone()
                })
                .collect::<Vec<_>>(),
            &scorer::Alloc::Equal,
            &self.costs,
        );
        let bhm = scorer::metrics(pb, &ew_bh.sim, warm, n);
        let ikey = crate::extract::idea_key(&code);
        let phash = params_hash(&json!({"params": params, "symbols": symbols, "alloc": alloc}));
        let label = format!("PORTFOLIO:{}", symbols.join("+"));
        let all = self.store.record_trial(&NewTrial {
            strategy_id: &s.id,
            job_id: &run_id,
            idea_key: &ikey,
            params_hash: &phash,
            symbol: &label,
            dataset_sha: &scored
                .iter()
                .map(|x| {
                    x.result["dataset_sha256"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(","),
            bars: pm.bars,
            sharpe_daily: pm.sharpe_daily,
            positions: &ps.sim.exposure,
            lookahead: false,
            run_kind: "portfolio",
            group_id: Some(&run_id),
        })?;
        let (dsr, info) = self.dsr_for(&all, &pm);
        let probe = ProbeReport {
            detected: false,
            variants: scored.iter().map(|x| x.probe.variants).sum(),
            containers: scored.iter().map(|x| x.probe.containers).sum(),
            order: vec![],
            mismatches: vec![],
        };
        let validated = scored.iter().fold((true, String::new()), |acc, x| {
            (
                acc.0 && x.validated.0,
                format!("{}{}: {}; ", acc.1, x.symbol, x.validated.1),
            )
        });
        let high: Vec<String> = scored
            .iter()
            .flat_map(|x| x.high_risks.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let g = gate::evaluate(
            &gate::GateInput {
                job_status: "succeeded",
                hardened: exec.hardened(),
                runner_identity: scored[0].identity.as_deref(),
                dev: &pm,
                dev_cost_2x: &pm2,
                folds: &folds,
                dsr: &dsr,
                probe: &probe,
                data_adjusted: scored.iter().all(|x| x.adjusted),
                data_validated: validated,
                high_risks: &high,
                buy_hold: &bhm,
                expected_runner: self.expected_runner_id().as_deref(),
                synthetic: scored.iter().any(|x| x.synthetic),
                capacity: scored
                    .iter()
                    .map(|x| x.capacity)
                    .collect::<Option<Vec<f64>>>()
                    .and_then(|v| v.into_iter().reduce(f64::min)),
            },
            &self.gate,
        );
        let eq_path = self.home.root.join("runs").join(&run_id);
        std::fs::create_dir_all(&eq_path)?;
        let mut csv = String::from("date,equity,equity_cost2x,gross_exposure");
        for l in &legs_in {
            csv.push_str(&format!(",w_{}", l.symbol));
        }
        csv.push('\n');
        for t in 0..n {
            csv.push_str(&format!(
                "{},{:.2},{:.2},{:.4}",
                dates[t], ps.sim.equity[t], ps2.sim.equity[t], ps.sim.exposure[t]
            ));
            for w in &ps.weights {
                csv.push_str(&format!(",{:.4}", w[t]));
            }
            csv.push('\n');
        }
        std::fs::write(eq_path.join("equity.csv"), csv)?;
        let result = json!({"kind": "portfolio", "computed_by": "cmsq-core scorer (host, trusted)", "symbols": symbols, "alloc": alloc, "params": params,
            "window": {"start": dates[0], "end": dates[n - 1], "bars": n, "warmup_bars": warm, "note": "common development dates only; vault bars never used"},
            "portfolio": pm, "portfolio_cost_2x": pm2, "walk_forward_folds": folds, "equal_weight_buy_and_hold": bhm, "trades": ps.sim.trades, "costs_paid": ps.sim.costs_paid,
            "blocked": {"limit_up": ps.sim.blocked_limit_up, "limit_down": ps.sim.blocked_limit_down, "suspended": ps.sim.blocked_suspended},
            "legs": scored.iter().map(|x| json!({"symbol": x.symbol, "job": x.job_id, "development": x.dm, "dsr": x.dsr.dsr})).collect::<Vec<_>>(),
            "leg_return_correlation": scorer::leg_correlation(&legs_in),
            "dsr": dsr, "trial_info": info, "gate": {"passed": g.passed, "reasons": g.reasons, "checks": g.checks, "note": "informational: portfolios are not promotable in P1"},
            "equity_csv": eq_path.join("equity.csv").display().to_string()});
        std::fs::write(
            eq_path.join("result.json"),
            serde_json::to_string_pretty(&result)?,
        )?;
        self.store.finish_run(&run_id, "succeeded", &result)?;
        self.store.audit("gate", "portfolio_evaluate", Some(&s.id), json!({"run": run_id, "passed": g.passed, "reasons": g.reasons, "sharpe": pm.sharpe, "dsr": dsr.dsr, "trials": dsr.trials}), true)?;
        self.log(&format!(
            "portfolio {}: Sharpe {:.2}, DSR {:.3} (N={}), gate {}",
            &run_id[..8],
            pm.sharpe,
            dsr.dsr,
            dsr.trials,
            if g.passed { "pass" } else { "fail" }
        ));
        Ok(PortfolioSummary {
            run_id,
            strategy_id: s.id.clone(),
            passed: g.passed,
            reasons: g.reasons,
            result,
        })
    }

    // ---------------- promote (manual + one-time vault holdout) / reject ----------------
    /// `confirm` must be typed by a human: exactly `PROMOTE <first 8 chars of id>`.
    /// Then the strategy is run ONCE on the vault (bars never shown before). A vault
    /// failure rejects; the vault can't be retried for the same strategy or idea.
    pub async fn promote(
        &self,
        id: &str,
        actor: &str,
        confirm: &str,
        exec: &dyn SandboxExecutor,
    ) -> Result<Strategy> {
        let s = self.get(id)?;
        let phrase = Self::confirmation_phrase(&s);
        if confirm.trim() != phrase {
            self.store.audit(
                actor,
                "promote_denied",
                Some(&s.id),
                json!({"error": "confirmation phrase mismatch"}),
                false,
            )?;
            return Err(CoreError::ConfirmationMismatch { expected: phrase }.into());
        }
        let ev = match self.store.latest_evaluation(&s.id)? {
            Some(ev) if ev.passed && s.status == StrategyStatus::GatePassed => ev,
            _ => {
                self.store.audit(
                    actor,
                    "promote_denied",
                    Some(&s.id),
                    json!({"status": s.status, "error": "not GatePassed"}),
                    false,
                )?;
                return Err(CoreError::NotGatePassed {
                    id: s.id.clone(),
                    status: s.status,
                }
                .into());
            }
        };
        if !exec.hardened() && self.gate.require_hardened_backend {
            return Err(CoreError::BackendUnavailable(
                "promotion (vault run) requires the Docker backend".into(),
            )
            .into());
        }
        let dev_job = self
            .store
            .list_jobs(Some(&s.id))?
            .into_iter()
            .find(|j| j.id == ev.job_id)
            .context("evaluation job missing")?;
        let symbol = dev_job.symbol.clone();
        let (e, code) = Self::code_of(&s)?;
        let ikey = crate::extract::idea_key(&code);
        let csha = hex::encode(sha2::Sha256::digest(code.as_bytes()));
        let job_id = uuid::Uuid::new_v4().to_string();
        // ---- atomic one-shot claim: dedup check + insert under the vault lock ----
        let sid = s.id.clone();
        let claim =
            self.store
                .claim_vault(&s.id, &job_id, &ikey, &symbol, &csha, |touches, trials| {
                    let mine = trials
                        .iter()
                        .rev()
                        .find(|t| {
                            t.strategy_id == sid && t.symbol == symbol && t.run_kind != "portfolio"
                        })
                        .map(|t| t.positions.clone())
                        .unwrap_or_default();
                    touches.iter().find_map(|t| {
                        let same_idea = t.idea_key.as_deref() == Some(ikey.as_str())
                            && t.symbol.as_deref() == Some(symbol.as_str());
                        let same_code = t.code_sha.as_deref() == Some(csha.as_str());
                        let near = trials.iter().any(|x| {
                            x.strategy_id == t.strategy_id
                                && x.symbol == symbol
                                && x.positions.len() == mine.len()
                                && corr(&x.positions, &mine).is_some_and(|c| c > 0.9)
                        });
                        (same_idea || same_code || near).then(|| {
                            format!(
                                "{} already used the vault ({})",
                                &t.strategy_id[..8.min(t.strategy_id.len())],
                                if same_code {
                                    "identical code"
                                } else if same_idea {
                                    "same idea key + symbol"
                                } else {
                                    "near-duplicate positions (corr > 0.9)"
                                }
                            )
                        })
                    })
                })?;
        if let Err(why) = claim {
            self.store.audit(
                actor,
                "promote_denied",
                Some(&s.id),
                json!({"error": "vault spent", "detail": why}),
                false,
            )?;
            let s = self.get(&s.id)?;
            self.transition(
                &s,
                StrategyStatus::Rejected,
                actor,
                Some(&format!("vault holdout already spent: {why}")),
                json!({}),
            )?;
            return Err(CoreError::VaultSpent(why).into());
        }
        // the claim is recorded BEFORE running: a crash still burns the vault
        let ds = match self.load_dataset(&symbol).await {
            Ok(x) => x,
            Err(err) => {
                self.store.finish_vault(&s.id, false)?;
                return Err(err);
            }
        };
        let (bars, _meta, att) = ds;
        let dev_sha = dev_job
            .result
            .as_ref()
            .and_then(|r| r["dataset_sha256"].as_str())
            .unwrap_or_default()
            .to_string();
        if att["snapshot_sha256"].as_str() != Some(dev_sha.as_str()) {
            self.store.finish_vault(&s.id, false)?;
            return Err(CoreError::DataIntegrity(format!("{symbol} changed since the development run (dataset {} vs {}); the vault claim is spent", &dev_sha[..12.min(dev_sha.len())],
                att["snapshot_sha256"].as_str().unwrap_or("-"))).into());
        }
        let rules: scorer::Rules = serde_json::from_value(att["rules"].clone()).unwrap_or_default();
        let fut = crate::futures::spec_for(&symbol);
        let dev_end = ((bars.len() as f64) * self.dev_frac) as usize;
        let identity = exec.runner_identity(RUNNER_IMAGE).await;
        let jd = self.home.job_dir(&s.id, &job_id);
        std::fs::create_dir_all(&jd)?;
        self.store.insert_job(&JobRecord {
            id: job_id.clone(),
            strategy_id: s.id.clone(),
            backend: exec.name().into(),
            image: RUNNER_IMAGE.into(),
            symbol: symbol.clone(),
            data_source: dev_job.data_source.clone(),
            status: "running".into(),
            exit_code: None,
            started_at: Utc::now(),
            finished_at: None,
            duration_ms: None,
            job_dir: jd.display().to_string(),
            error: None,
            result: None,
            owner_pid: None,
            runner_identity: identity.clone(),
            kind: "vault".into(),
            owner_host: None,
        })?;
        self.store.audit(actor, "vault_start", Some(&s.id), json!({"job": job_id, "confirmed_by": actor, "vault": {"start": bars[dev_end].date, "end": bars.last().unwrap().date}}), true)?;
        let view = probe::container_view(&bars);
        let n = bars.len();
        let seed = Rng::from_uuid().next_u64();
        let mut rng = Rng::new(seed);
        let lim = match &fut {
            Some(f) => f.limit_pct - 0.005,
            None => scorer::limit_pct(&symbol, &bars[dev_end].date) - 0.005,
        };
        let vcfg = probe::ProbeConfig {
            lo: dev_end as f64 / n as f64,
            ..self.probe.clone()
        };
        let (mut rcuts, tcuts) = probe::choose_cuts(n, dev_end + 1, &vcfg, &mut rng);
        rcuts.insert(0, dev_end);
        let trunc: Vec<usize> = rcuts.iter().chain(&tcuts).copied().collect();
        let mcuts = probe::strata_cuts(n, dev_end + 1, &vcfg, &mut rng);
        let plan = probe::plan_with_mirrors(&view, &trunc, &rcuts, &mcuts, 1, lim, &mut rng);
        self.store.audit("sandbox", "vault_probe_plan", Some(&s.id), json!({"job": job_id, "seed": seed.to_string(), "random_cuts": rcuts, "tail_cuts": tcuts, "mirror_cuts": mcuts, "dataset_sha256": dev_sha}), true)?;
        // the vault runs the exact parameters that were gated (sweep winner included)
        let params = dev_job
            .result
            .as_ref()
            .and_then(|r| r.get("params").cloned())
            .filter(|p| p.is_object())
            .unwrap_or_else(|| Self::params_for(&e, None));
        let t0 = std::time::Instant::now();
        let outcome = self
            .run_plan(
                exec,
                &jd,
                &plan,
                &code,
                &params,
                (
                    if fut.is_some() { -1.0 } else { 0.0 },
                    fut.as_ref().map(|f| f.tick).unwrap_or(0.01),
                ),
            )
            .await;
        let ms = t0.elapsed().as_millis() as i64;
        let s = self.get(&s.id)?;
        let (full, rep) = match outcome {
            Ok((f, r, _)) => (f, r),
            Err((status, reason)) => {
                self.store
                    .finish_job(&job_id, &status, None, ms, Some(&reason), None)?;
                self.store.finish_vault(&s.id, false)?;
                self.transition(
                    &s,
                    StrategyStatus::Rejected,
                    "gate",
                    Some(&format!("vault run failed ({status}): {reason}")),
                    json!({"job": job_id}),
                )?;
                return self.get(&s.id);
            }
        };
        let (sim, sim2) = match &fut {
            Some(spec) => {
                let fx = crate::futures::FuturesCosts::default();
                (
                    crate::futures::simulate(spec, &bars, &full, &fx, &self.costs),
                    crate::futures::simulate(spec, &bars, &full, &fx, &self.costs.stressed(2.0)),
                )
            }
            None => (
                scorer::simulate_with(&symbol, &bars, &full, &self.costs, &rules),
                scorer::simulate_with(&symbol, &bars, &full, &self.costs.stressed(2.0), &rules),
            ),
        };
        let vm = scorer::metrics(&bars, &sim, dev_end, n);
        Self::write_equity(&jd.join("equity.csv"), &bars, &full, &sim, &sim2)?;
        let mut reasons = vec![];
        if rep.detected {
            reasons.push(format!(
                "vault look-ahead/consistency probe failed: {:?}",
                rep.mismatches.first()
            ));
        }
        if vm.total_return <= 0.0 || vm.sharpe <= 0.0 {
            reasons.push(format!(
                "vault holdout not profitable: return {:.2}%, Sharpe {:.2}",
                vm.total_return * 100.0,
                vm.sharpe
            ));
        }
        if !gate::runner_pinned(identity.as_deref(), self.expected_runner_id().as_deref()) {
            reasons.push("runner identity not pinned".into());
        }
        let passed = reasons.is_empty();
        let result = json!({"kind": "vault", "computed_by": "cmsq-core scorer (host, trusted)", "symbol": symbol, "params": params, "vault": vm, "lookahead_probe": rep,
            "runner_identity": identity, "execution": if fut.is_some() { crate::futures::EXECUTION_LABEL } else { scorer::EXECUTION_LABEL }, "passed": passed, "reasons": reasons,
            "latest_position": {"date": bars[n - 1].date, "target_position": full[n - 1]},
            "close_today": {"lots": sim.close_today_lots, "fees": sim.close_today_fees},
            "trade_stats": crate::tradestats::stats_window(&sim.fills, fut.as_ref().map(|f| f.multiplier).unwrap_or(1.0), dev_end),
            "account": crate::qifi::account_inputs(&symbol, fut.as_ref(), &sim, &bars, dev_end)});
        std::fs::write(
            jd.join("result.json"),
            serde_json::to_string_pretty(&result)?,
        )?;
        self.store
            .finish_job(&job_id, "succeeded", Some(0), ms, None, Some(&result))?;
        self.store.finish_vault(&s.id, passed)?;
        self.store.audit(
            "gate",
            "vault_evaluate",
            Some(&s.id),
            json!({"job": job_id, "passed": passed, "reasons": reasons, "vault": vm}),
            true,
        )?;
        if !passed {
            self.transition(
                &s,
                StrategyStatus::Rejected,
                "gate",
                Some(&reasons.join("; ")),
                json!({"job": job_id}),
            )?;
            return self.get(&s.id);
        }
        self.transition(
            &s,
            StrategyStatus::Promoted,
            actor,
            Some("manually confirmed; vault holdout passed"),
            json!({"evaluation": ev.id, "dev_job": ev.job_id, "vault_job": job_id}),
        )?;
        let sig = Signal {
            strategy_id: s.id.clone(),
            job_id: job_id.clone(),
            symbol: symbol.clone(),
            date: bars[n - 1].date.clone(),
            target_position: full[n - 1],
            created_at: Utc::now(),
        };
        self.store.insert_signal(&sig)?;
        self.store.audit(
            "system",
            "signal_observe_only",
            Some(&s.id),
            json!({"signal": sig, "note": "observe-only; no broker connected"}),
            true,
        )?;
        self.get(&s.id)
    }

    /// Optional QIFI-style export of a PROMOTED strategy's observe-only paper
    /// account (vault holdout window) and its signals. Written to
    /// `<home>/runs/qifi_<id8>_<ts>.json` (or `out`) and audited with its sha256.
    pub fn export_qifi(
        &self,
        id: &str,
        out: Option<&Path>,
    ) -> Result<(PathBuf, serde_json::Value)> {
        let s = self.get(id)?;
        anyhow::ensure!(
            s.status == StrategyStatus::Promoted,
            "QIFI export is only for Promoted strategies ({} is {})",
            &s.id[..8],
            s.status
        );
        let jobs = self.store.list_jobs(Some(&s.id))?;
        let vault = jobs
            .iter()
            .find(|j| {
                j.kind == "vault"
                    && j.status == "succeeded"
                    && j.result.as_ref().is_some_and(|r| r["passed"] == true)
            })
            .ok_or_else(|| anyhow::anyhow!("no passed vault job for {}", &s.id[..8]))?;
        let r = vault.result.as_ref().unwrap();
        anyhow::ensure!(
            r["account"].is_object(),
            "vault job {} predates account snapshots (older cmsq); no QIFI data",
            &vault.id[..8]
        );
        let sigs: Vec<Signal> = self
            .store
            .list_signals()?
            .into_iter()
            .map(|x| x.0)
            .filter(|g| g.strategy_id == s.id)
            .collect();
        let doc = crate::qifi::build(
            &s.id,
            &s.title,
            &r["account"],
            &sigs,
            &chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        );
        let path = match out {
            Some(p) => p.to_path_buf(),
            None => {
                let d = self.home.root.join("runs");
                std::fs::create_dir_all(&d)?;
                d.join(format!(
                    "qifi_{}_{}.json",
                    &s.id[..8],
                    chrono::Local::now().format("%Y%m%dT%H%M%S")
                ))
            }
        };
        let text = serde_json::to_string_pretty(&doc)?;
        std::fs::write(&path, &text)?;
        self.store.audit("user", "qifi_export", Some(&s.id), json!({"path": path, "vault_job": vault.id, "sha256": crate::calendars::sha256_hex(text.as_bytes()), "observe_only": true}), true)?;
        Ok((path, doc))
    }

    pub fn reject(&self, id: &str, actor: &str, reason: &str) -> Result<Strategy> {
        let s = self.get(id)?;
        self.transition(
            &s,
            StrategyStatus::Rejected,
            actor,
            Some(reason),
            json!({"manual": true}),
        )?;
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

/// A verified dataset (bars from the hashed snapshot + its audited attestation).
pub struct Dataset {
    pub symbol: String,
    pub bars: Vec<Bar>,
    pub meta: market::DataMeta,
    pub att: serde_json::Value,
}

pub struct Scored {
    pub job_id: String,
    pub symbol: String,
    pub params: serde_json::Value,
    pub dm: Metrics,
    pub dm2: Metrics,
    pub folds: Vec<Metrics>,
    pub bhm: Metrics,
    pub probe: ProbeReport,
    pub adjusted: bool,
    pub validated: (bool, String),
    pub identity: Option<String>,
    pub hardened: bool,
    pub dsr: scorer::Dsr,
    pub positions: Vec<f64>,
    pub dev_bars: Vec<Bar>,
    pub result: serde_json::Value,
    pub high_risks: Vec<String>,
    pub equity: Vec<f64>,
    pub synthetic: bool,
    pub capacity: Option<f64>,
}

pub enum RunOutcome {
    Scored(Box<Scored>),
    Failed {
        job_id: String,
        status: String,
        reason: String,
    },
}

#[derive(Debug, serde::Serialize)]
pub struct SweepSummary {
    pub run_id: String,
    pub strategy_id: String,
    pub status: StrategyStatus,
    pub best_job: Option<String>,
    pub best_params: Option<serde_json::Value>,
    pub points: Vec<serde_json::Value>,
    pub dsr: Option<scorer::Dsr>,
    pub reasons: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct PortfolioSummary {
    pub run_id: String,
    pub strategy_id: String,
    pub passed: bool,
    pub reasons: Vec<String>,
    pub result: serde_json::Value,
}

/// Hard cap on grid size (each point costs ~15 containers and one trial).
pub const MAX_SWEEP_POINTS: usize = 64;

pub fn params_hash(p: &serde_json::Value) -> String {
    // serde_json maps are ordered (BTreeMap), so the text is canonical
    hex::encode(&sha2::Sha256::digest(p.to_string().as_bytes())[..8])
}

/// Cartesian product of `grid` (name -> array of values). Only parameters the
/// extractor found may be swept (unknown names are refused, not silently added).
pub fn expand_grid(
    e: &ExtractedStrategy,
    grid: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<serde_json::Value>> {
    anyhow::ensure!(!grid.is_empty(), "empty sweep grid");
    let mut pts = vec![serde_json::Map::new()];
    for (k, v) in grid {
        anyhow::ensure!(
            e.parameters.contains_key(k),
            "sweep parameter {k:?} is not a parameter of this strategy (known: {:?})",
            e.parameters.keys().collect::<Vec<_>>()
        );
        let vals = v
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| anyhow::anyhow!("sweep values for {k} must be a non-empty array"))?;
        let mut next = vec![];
        for p in &pts {
            for x in vals {
                anyhow::ensure!(
                    x.is_number() || x.is_boolean() || x.is_string(),
                    "sweep value for {k} must be a scalar"
                );
                let mut q = p.clone();
                q.insert(k.clone(), x.clone());
                next.push(q);
            }
        }
        pts = next;
        anyhow::ensure!(
            pts.len() <= MAX_SWEEP_POINTS,
            "sweep grid exceeds {MAX_SWEEP_POINTS} points"
        );
    }
    let uniq: std::collections::BTreeSet<String> = pts
        .iter()
        .map(|p| serde_json::Value::Object(p.clone()).to_string())
        .collect();
    anyhow::ensure!(uniq.len() == pts.len(), "sweep grid has duplicate points");
    Ok(pts.into_iter().map(serde_json::Value::Object).collect())
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl Pipe for serde_json::Value {}

/// Sources whose fetches are independent real-market downloads. Only the signed
/// (audited, HMAC-chained) fetch attestation decides the class; the editable
/// `<symbol>.meta.json` never does (issue #5). Anything else (bundled samples,
/// user CSV imports, unknown or missing source) fails closed as non-real.
pub const REAL_DATA_SOURCES: &[&str] = &["tushare", "rstdx"];

pub fn attested_real_data(att: &serde_json::Value, symbol: &str) -> bool {
    let src = att["source"].as_str().unwrap_or("");
    REAL_DATA_SOURCES.contains(&src) && !symbol.starts_with("SAMPLE_")
}
