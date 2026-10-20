//! akshare / baostock via a host-side python helper (scripts/data_helper.py, embedded).
//! Serial per source with a minimum spacing (akshare ≥1 s, Eastmoney is scraped),
//! hard timeout, retries with jittered exponential backoff.
use super::ratelimit::{backoff, jitter};
use crate::market::{Bar, DataMeta, MarketDataSource};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const HELPER: &str = include_str!("../../../../scripts/data_helper.py");

pub struct PyProvider {
    pub kind: &'static str,
    pub python: String,
    pub min_spacing: Duration,
    pub timeout: Duration,
    pub retries: u32,
    last: Mutex<Option<Instant>>,
}

impl PyProvider {
    pub fn new(kind: &'static str) -> Self {
        let python = std::env::var("CMSQ_PYTHON").unwrap_or_else(|_| "python3".into());
        let (sp, to) = match kind {
            "akshare" => (Duration::from_millis(1200), Duration::from_secs(60)),
            _ => (Duration::from_millis(500), Duration::from_secs(45)),
        };
        Self { kind, python, min_spacing: sp, timeout: to, retries: 2, last: Mutex::new(None) }
    }

    async fn call(&self, symbol: &str, start: &str, end: &str) -> Result<serde_json::Value> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("data_helper.py");
        std::fs::write(&script, HELPER)?;
        let mut last_err = None;
        for attempt in 0..=self.retries {
            {
                // serialize calls to this source and enforce spacing (+jitter)
                let mut g = self.last.lock().await;
                if let Some(t) = *g {
                    let gap = jitter(self.min_spacing, 0.25);
                    if t.elapsed() < gap {
                        tokio::time::sleep(gap - t.elapsed()).await;
                    }
                }
                // third-party packages with network access get a scrubbed env: no
                // TUSHARE_TOKEN, no LLM keys, nothing but what python + egress need
                let mut cmd = tokio::process::Command::new(&self.python);
                cmd.env_clear().envs(scrubbed_env(std::env::vars()));
                let out = tokio::time::timeout(self.timeout, cmd.arg(&script).args([self.kind, symbol, start, end]).kill_on_drop(true).output()).await;
                *g = Some(Instant::now());
                match out {
                    Ok(Ok(o)) if o.status.success() => return serde_json::from_slice(&o.stdout).context("helper output not json"),
                    Ok(Ok(o)) => last_err = Some(anyhow::anyhow!("{} helper failed: {}", self.kind,
                        String::from_utf8_lossy(&o.stderr).lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(300).collect::<String>())),
                    Ok(Err(e)) => return Err(anyhow::anyhow!("cannot run {} ({e}); set CMSQ_PYTHON to a python with {} installed", self.python, self.kind)),
                    Err(_) => last_err = Some(anyhow::anyhow!("{} helper timed out after {:?}", self.kind, self.timeout)),
                }
            }
            if attempt < self.retries {
                tokio::time::sleep(backoff(Duration::from_secs(2), attempt)).await;
            }
        }
        Err(last_err.unwrap())
    }
}

#[async_trait]
impl MarketDataSource for PyProvider {
    fn name(&self) -> &'static str {
        self.kind
    }
    async fn contract_closes(&self, segments: &[(String, String, String)]) -> Result<Vec<Bar>> {
        anyhow::ensure!(self.kind == "akshare", "{} has no per-contract futures endpoint", self.kind);
        let mut out = vec![];
        let mut failed: Vec<String> = vec![];
        for (c, a, b) in segments {
            anyhow::ensure!(c.len() <= 8 && c.chars().all(|x| x.is_ascii_alphanumeric()), "bad contract code");
            // sina only keeps roughly the last five years of expired contracts: an old
            // segment that cannot be fetched is skipped (it simply stays unvalidated)
            let v = match self.call(&format!("FUT:{c}"), &a.replace('-', ""), &b.replace('-', "")).await {
                Ok(v) => v,
                Err(e) => {
                    failed.push(format!("{c}: {e:#}"));
                    continue;
                }
            };
            for r in v["bars"].as_array().context("no bars")? {
                let Some(r) = r.as_array() else { continue };
                let (Some(d), Some(cl)) = (r[0].as_str(), r[4].as_f64()) else { continue };
                if d >= a.as_str() && d <= b.as_str() {
                    out.push(Bar { date: d.into(), open: r[1].as_f64().unwrap_or(cl), high: r[2].as_f64().unwrap_or(cl), low: r[3].as_f64().unwrap_or(cl), close: cl,
                        volume: r[5].as_f64().unwrap_or(0.0), adj_factor: None });
                }
            }
        }
        anyhow::ensure!(!out.is_empty(), "no contract segment could be fetched ({} tried): {}", segments.len(), failed.first().cloned().unwrap_or_default());
        out.sort_by(|a, b| a.date.cmp(&b.date));
        Ok(out)
    }
    async fn daily_bars(&self, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta)> {
        anyhow::ensure!(!crate::futures::is_futures(symbol), "{}: futures main series come from tushare; akshare validates per contract", self.kind);
        crate::market::parse_ashare_symbol(symbol)?;
        let today = chrono::Local::now().date_naive();
        let start = today - chrono::Duration::days(((count.min(20_000) as f64) * 1.5) as i64 + 30);
        let v = self.call(&symbol.to_uppercase(), &start.format("%Y%m%d").to_string(), &today.format("%Y%m%d").to_string()).await?;
        let mut bars: Vec<Bar> = v["bars"].as_array().context("no bars")?.iter().filter_map(|r| {
            let r = r.as_array()?;
            Some(Bar { date: r[0].as_str()?.into(), open: r[1].as_f64()?, high: r[2].as_f64()?, low: r[3].as_f64()?, close: r[4].as_f64()?, volume: r[5].as_f64()?, adj_factor: None })
        }).filter(|b| [b.open, b.high, b.low, b.close].iter().all(|x| x.is_finite() && *x > 0.0)).collect();
        bars.sort_by(|a, b| a.date.cmp(&b.date));
        bars.dedup_by(|a, b| a.date == b.date);
        if bars.len() > count {
            bars.drain(..bars.len() - count);
        }
        anyhow::ensure!(!bars.is_empty(), "{} returned no bars for {symbol}", self.kind);
        Ok((bars.clone(), DataMeta { symbol: symbol.to_uppercase(), source: self.kind.into(), label: format!("REAL A-SHARE DAILY ({})", self.kind),
            fetched_at: chrono::Utc::now().to_rfc3339(), start: bars[0].date.clone(), end: bars.last().unwrap().date.clone(), bars: bars.len(),
            adjustment: "none (unadjusted; backup / validation source)".into(), server: None, extra: v["meta"].clone() }))
    }
}

/// Environment passed to third-party data helpers: only what python and HTTP
/// egress need. Never tokens / API keys / CMSQ_* settings.
pub fn scrubbed_env(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    vars.filter(|(k, _)| {
        let ku = k.to_uppercase();
        let secretish = ["TOKEN", "KEY", "SECRET", "PASSWORD", "CMSQ_", "TUSHARE"].iter().any(|w| ku.contains(w));
        !secretish && (matches!(ku.as_str(), "PATH" | "HOME" | "LANG" | "LC_ALL" | "TZ" | "SSL_CERT_FILE" | "SSL_CERT_DIR" | "REQUESTS_CA_BUNDLE") || ku.contains("PROXY"))
    }).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn helper_env_is_scrubbed() {
        let vars = [("TUSHARE_TOKEN", "c1"), ("OPENAI_API_KEY", "c2"), ("CMSQ_LLM_KEY", "c3"), ("HTTPS_PROXY", "http://p"), ("PROXY_TOKEN", "c4"),
            ("PATH", "/usr/bin"), ("AWS_SECRET_ACCESS_KEY", "c5"), ("RANDOM", "x")].map(|(k, v)| (k.to_string(), v.to_string()));
        let env = super::scrubbed_env(vars.into_iter());
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["HTTPS_PROXY", "PATH"]);
    }
}
