//! Multi-source router: priority order with fallback, per-source health and
//! cooldown (persisted), cross-source close validation, provenance in meta.
use crate::market::{Bar, DataMeta, MarketDataSource};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

/// Closes differing by more than this (relative) on the same day are flagged.
pub const DIVERGENCE_THRESHOLD: f64 = 0.001;
pub const MIN_OVERLAP: usize = 20;
/// Any single day beyond this relative difference blocks the dataset.
pub const HARD_DIVERGENCE: f64 = 0.01;
/// Up to this many flagged (0.1%..1%) days are tolerated, or 0.2% of the overlap if larger.
pub const MAX_FLAGGED_DAYS: usize = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceHealth {
    pub ok: u32,
    pub fail: u32,
    pub consecutive_fail: u32,
    pub last_ok: Option<String>,
    pub last_error: Option<String>,
    pub cooldown_until: Option<String>,
    pub last_ms: Option<u64>,
}

pub struct Router {
    pub sources: Vec<Arc<dyn MarketDataSource>>,
    pub health_file: PathBuf,
    pub validate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Validation {
    pub status: String, // ok | divergent | unvalidated
    pub against: Option<String>,
    pub overlap_days: usize,
    pub divergent_days: usize,
    pub max_rel_diff: f64,
    pub threshold: f64,
    pub divergent_sample: Vec<Value>,
    pub note: String,
}

/// Compare raw closes on overlapping dates. Suspended (volume 0) filler bars are skipped.
pub fn compare(a: &[Bar], b: &[Bar], against: &str) -> Validation {
    let bm: HashMap<&str, &Bar> = b.iter().map(|x| (x.date.as_str(), x)).collect();
    let mut overlap = 0;
    let mut div = vec![];
    let mut max_rel: f64 = 0.0;
    for x in a.iter().filter(|x| x.volume > 0.0) {
        if let Some(y) = bm.get(x.date.as_str()).filter(|y| y.volume > 0.0) {
            overlap += 1;
            let rel = (x.close - y.close).abs() / x.close.abs().max(1e-9);
            max_rel = max_rel.max(rel);
            if rel > DIVERGENCE_THRESHOLD {
                div.push(json!({"date": x.date, "primary": x.close, "other": y.close, "rel": rel}));
            }
        }
    }
    // policy: isolated small differences are recorded and tolerated; many, or any large one, block
    let budget = MAX_FLAGGED_DAYS.max(overlap / 500);
    let status = if overlap < MIN_OVERLAP {
        "unvalidated"
    } else if div.is_empty() {
        "ok"
    } else if max_rel <= HARD_DIVERGENCE && div.len() <= budget {
        "ok_with_flags"
    } else {
        "divergent"
    };
    let note = format!("{} of {overlap} overlapping closes differ by >{:.1}% (policy: block if >{budget} such days or any day >{:.0}%; max {:.3}%)",
        div.len(), DIVERGENCE_THRESHOLD * 100.0, HARD_DIVERGENCE * 100.0, max_rel * 100.0);
    Validation { status: status.into(), against: Some(against.into()), overlap_days: overlap, divergent_days: div.len(), max_rel_diff: max_rel,
        threshold: DIVERGENCE_THRESHOLD, note, divergent_sample: div.into_iter().take(20).collect() }
}

/// Gate view of a cached dataset: (ok, detail). Synthetic samples pass; real
/// data must be cross-validated with no divergent day.
pub fn validation_status(meta: &DataMeta) -> (bool, String) {
    match meta.source.as_str() {
        "sample" if meta.symbol.starts_with("SAMPLE_") => return (true, "bundled synthetic sample (hash-checked against the binary)".into()),
        "sample" => return (false, "source=sample on a non-SAMPLE symbol: refused".into()),
        "csv" => return (false, "user-supplied CSV: not cross-validated against an independent source; real-data checks cannot pass".into()),
        _ => {}
    }
    match meta.extra.get("validation").and_then(|v| serde_json::from_value::<Validation>(v.clone()).ok()) {
        Some(v) if v.status == "ok_with_flags" => (true, format!("{} vs {}: {} — tolerated and recorded", meta.source, v.against.clone().unwrap_or_default(), v.note)),
        Some(v) if v.status == "ok" => (true, format!("{} vs {}: {} overlapping closes agree within {:.1}% (max diff {:.4}%)",
            meta.source, v.against.unwrap_or_default(), v.overlap_days, v.threshold * 100.0, v.max_rel_diff * 100.0)),
        Some(v) if v.status == "divergent" => (false, format!("{} vs {}: {} — dataset blocked; investigate before trusting", meta.source, v.against.unwrap_or_default(), v.note)),
        Some(v) => (false, format!("not cross-validated: {}", v.note)),
        None => (false, "not cross-validated (no second source reachable); real data must agree with an independent source".into()),
    }
}

fn set(extra: &mut Value, k: &str, v: Value) {
    if !extra.is_object() {
        *extra = json!({"original": extra.clone()});
    }
    extra[k] = v;
}

impl Router {
    pub fn load_health(&self) -> BTreeMap<String, SourceHealth> {
        std::fs::read_to_string(&self.health_file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    fn save_health(&self, h: &BTreeMap<String, SourceHealth>) {
        if let Some(p) = self.health_file.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let _ = std::fs::write(&self.health_file, serde_json::to_string_pretty(h).unwrap_or_default());
    }
    fn record(&self, name: &str, r: Result<u64, String>) {
        let mut h = self.load_health();
        let e = h.entry(name.into()).or_default();
        let now = chrono::Utc::now();
        match r {
            Ok(ms) => {
                e.ok += 1;
                e.consecutive_fail = 0;
                e.last_ok = Some(now.to_rfc3339());
                e.cooldown_until = None;
                e.last_ms = Some(ms);
            }
            Err(err) => {
                e.fail += 1;
                e.consecutive_fail += 1;
                e.last_error = Some(err.chars().take(300).collect());
                let secs = (30u64 << e.consecutive_fail.min(6)).min(1800);
                e.cooldown_until = Some((now + chrono::Duration::seconds(secs as i64)).to_rfc3339());
            }
        }
        self.save_health(&h);
    }
    fn cooling(&self, name: &str) -> Option<String> {
        let h = self.load_health();
        let until = h.get(name)?.cooldown_until.clone()?;
        let t = chrono::DateTime::parse_from_rfc3339(&until).ok()?;
        (t > chrono::Utc::now()).then_some(until)
    }

    async fn try_one(&self, s: &Arc<dyn MarketDataSource>, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta), String> {
        let t0 = std::time::Instant::now();
        let r = tokio::time::timeout(std::time::Duration::from_secs(600), s.daily_bars(symbol, count)).await
            .map_err(|_| "timed out".to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
        self.record(s.name(), r.as_ref().map(|_| t0.elapsed().as_millis() as u64).map_err(|e| e.clone()));
        r
    }

    pub async fn fetch(&self, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta)> {
        let mut attempts = vec![];
        let mut got = None;
        for s in &self.sources {
            if let Some(u) = self.cooling(s.name()) {
                attempts.push(json!({"source": s.name(), "skipped": format!("cooldown until {u}")}));
                continue;
            }
            match self.try_one(s, symbol, count).await {
                Ok(x) => {
                    attempts.push(json!({"source": s.name(), "ok": true}));
                    got = Some((s.name(), x));
                    break;
                }
                Err(e) => {
                    eprintln!("[data] {} failed for {symbol}: {e}", s.name());
                    attempts.push(json!({"source": s.name(), "ok": false, "error": e}));
                }
            }
        }
        let Some((primary, (bars, mut meta))) = got else { anyhow::bail!("all sources failed for {symbol}: {}", Value::from(attempts)) };
        let mut validation = Validation { status: "unvalidated".into(), against: None, overlap_days: 0, divergent_days: 0, max_rel_diff: 0.0,
            threshold: DIVERGENCE_THRESHOLD, divergent_sample: vec![], note: "no validator attempted".into() };
        let mut vattempts = vec![];
        if self.validate && crate::futures::is_futures(symbol) {
            // futures: compare the held contract's raw closes, segment by segment
            let segs: Vec<(String, String, String)> = meta.extra.get("segments").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
            for s in self.sources.iter().filter(|s| s.name() != primary) {
                match s.contract_closes(&segs).await {
                    Ok(other) => {
                        let raw: Vec<Bar> = bars.iter().map(|b| Bar { adj_factor: None, ..b.clone() }).collect();
                        validation = compare(&raw, &other, s.name());
                        vattempts.push(json!({"source": s.name(), "ok": true, "status": validation.status, "segments": segs.len()}));
                        if validation.status != "unvalidated" {
                            break;
                        }
                    }
                    Err(e) => vattempts.push(json!({"source": s.name(), "ok": false, "error": format!("{e:#}")})),
                }
            }
            if validation.against.is_none() {
                validation.note = "no independent per-contract source reachable".into();
            }
        } else if self.validate {
            for s in self.sources.iter().filter(|s| s.name() != primary) {
                if self.cooling(s.name()).is_some() {
                    vattempts.push(json!({"source": s.name(), "skipped": "cooldown"}));
                    continue;
                }
                match self.try_one(s, symbol, count).await {
                    Ok((other, _)) => {
                        validation = compare(&bars, &other, s.name());
                        vattempts.push(json!({"source": s.name(), "ok": true, "status": validation.status}));
                        if validation.status != "unvalidated" {
                            break;
                        }
                    }
                    Err(e) => vattempts.push(json!({"source": s.name(), "ok": false, "error": e})),
                }
            }
            if validation.against.is_none() {
                validation.note = "no independent source reachable".into();
            }
        }
        set(&mut meta.extra, "validation", serde_json::to_value(&validation)?);
        set(&mut meta.extra, "router", json!({"priority": self.sources.iter().map(|s| s.name()).collect::<Vec<_>>(), "attempts": attempts, "validators": vattempts}));
        Ok((bars, meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    fn bars(n: usize, bump: Option<usize>) -> Vec<Bar> {
        (0..n).map(|i| Bar { date: format!("2024-01-{:02}", i + 1), open: 10.0, high: 10.0, low: 10.0,
            close: if Some(i) == bump { 10.0 + i as f64 * 0.01 + 0.5 } else { 10.0 + i as f64 * 0.01 }, volume: 1000.0, adj_factor: Some(1.0) }).collect()
    }
    struct Fake(&'static str, Option<Vec<Bar>>);
    #[async_trait]
    impl MarketDataSource for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn daily_bars(&self, s: &str, _c: usize) -> Result<(Vec<Bar>, DataMeta)> {
            let b = self.1.clone().ok_or_else(|| anyhow::anyhow!("down"))?;
            Ok((b.clone(), DataMeta { symbol: s.into(), source: self.0.into(), label: "x".into(), fetched_at: "".into(), start: "".into(), end: "".into(),
                bars: b.len(), adjustment: "adj_factor".into(), server: None, extra: json!({}) }))
        }
    }
    fn router(d: &tempfile::TempDir, s: Vec<Arc<dyn MarketDataSource>>) -> Router {
        Router { sources: s, health_file: d.path().join("h.json"), validate: true }
    }
    #[tokio::test]
    async fn fallback_validation_and_cooldown() {
        let d = tempfile::tempdir().unwrap();
        let r = router(&d, vec![Arc::new(Fake("a", None)), Arc::new(Fake("b", Some(bars(30, None)))), Arc::new(Fake("c", Some(bars(30, None))))]);
        let (_, m) = r.fetch("SH600000", 30).await.unwrap();
        assert_eq!(m.source, "b");
        assert_eq!(m.extra["validation"]["status"], "ok");
        assert_eq!(m.extra["validation"]["against"], "c");
        assert!(validation_status(&m).0);
        let h = r.load_health();
        assert_eq!(h["a"].consecutive_fail, 1);
        assert!(r.cooling("a").is_some());
        // second fetch skips a (cooldown)
        let (_, m) = r.fetch("SH600000", 30).await.unwrap();
        assert!(m.extra["router"]["attempts"][0]["skipped"].as_str().unwrap().contains("cooldown"));
    }
    #[tokio::test]
    async fn divergence_blocks_and_missing_validator_blocks() {
        let d = tempfile::tempdir().unwrap();
        let r = router(&d, vec![Arc::new(Fake("a", Some(bars(30, None)))), Arc::new(Fake("b", Some(bars(30, Some(7)))))]);
        let (_, m) = r.fetch("SH600000", 30).await.unwrap();
        assert_eq!(m.extra["validation"]["status"], "divergent");
        assert_eq!(m.extra["validation"]["divergent_days"], 1);
        assert!(!validation_status(&m).0);
        // one isolated 0.3% day is tolerated + recorded; four such days block
        let a = bars(40, None);
        let mut b = a.clone();
        b[5].close *= 1.003;
        assert_eq!(compare(&a, &b, "x").status, "ok_with_flags");
        for i in [7, 9, 11] {
            b[i].close *= 1.003;
        }
        assert_eq!(compare(&a, &b, "x").status, "divergent");
        let d2 = tempfile::tempdir().unwrap();
        let r = router(&d2, vec![Arc::new(Fake("a", Some(bars(30, None)))), Arc::new(Fake("b", None))]);
        let (_, m) = r.fetch("SH600000", 30).await.unwrap();
        assert_eq!(m.extra["validation"]["status"], "unvalidated");
        assert!(!validation_status(&m).0);
    }
}
