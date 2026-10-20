//! Pluggable market-data input. Data is fetched on the HOST (outside the sandbox)
//! and cached into `data/` which is mounted READ-ONLY into backtest containers.
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bar {
    pub date: String, // YYYY-MM-DD
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataMeta {
    pub symbol: String,
    pub source: String,
    pub label: String,
    pub fetched_at: String,
    pub start: String,
    pub end: String,
    pub bars: usize,
    pub adjustment: String,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub extra: serde_json::Value,
}

#[async_trait]
pub trait MarketDataSource: Send + Sync {
    fn name(&self) -> &'static str;
    /// Fetch up to `count` most-recent daily bars, oldest first.
    async fn daily_bars(&self, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta)>;
}

/// Offline fallback: bundled SYNTHETIC sample series (see scripts/gen_sample_data.py).
pub struct SampleSource;

const SAMPLES: &[(&str, &str, &str)] = &[
    ("SAMPLE_TREND", include_str!("../../../fixtures/market/SAMPLE_TREND.csv"), include_str!("../../../fixtures/market/SAMPLE_TREND.meta.json")),
    ("SAMPLE_RANGE", include_str!("../../../fixtures/market/SAMPLE_RANGE.csv"), include_str!("../../../fixtures/market/SAMPLE_RANGE.meta.json")),
];

impl SampleSource {
    pub fn symbols() -> Vec<&'static str> {
        SAMPLES.iter().map(|s| s.0).collect()
    }
}

pub fn parse_csv(csv: &str) -> Result<Vec<Bar>> {
    let mut out = vec![];
    for (i, line) in csv.lines().enumerate().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        anyhow::ensure!(f.len() >= 6, "bad csv line {i}");
        out.push(Bar { date: f[0].into(), open: f[1].parse()?, high: f[2].parse()?, low: f[3].parse()?, close: f[4].parse()?, volume: f[5].parse()? });
    }
    Ok(out)
}

#[async_trait]
impl MarketDataSource for SampleSource {
    fn name(&self) -> &'static str {
        "sample"
    }
    async fn daily_bars(&self, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta)> {
        let (_, csv, meta) = SAMPLES.iter().find(|s| s.0 == symbol)
            .with_context(|| format!("no sample series {symbol}; available: {:?}", Self::symbols()))?;
        let mut bars = parse_csv(csv)?;
        if bars.len() > count {
            bars.drain(..bars.len() - count);
        }
        let m: serde_json::Value = serde_json::from_str(meta)?;
        Ok((bars.clone(), DataMeta {
            symbol: symbol.into(), source: "sample".into(),
            label: m["label"].as_str().unwrap_or("SAMPLE").into(), fetched_at: chrono::Utc::now().to_rfc3339(),
            start: bars.first().map(|b| b.date.clone()).unwrap_or_default(), end: bars.last().map(|b| b.date.clone()).unwrap_or_default(),
            bars: bars.len(), adjustment: "n/a (synthetic)".into(), server: None, extra: m,
        }))
    }
}

/// Real A-share daily K-lines via the user's `rstdx` crate (TDX protocol), host-side only.
/// Symbols: `SH600000`, `SZ000001`, `BJ430047`.
#[cfg(feature = "tdx")]
pub struct RstdxSource {
    pub server: Option<String>,
}

pub fn parse_ashare_symbol(sym: &str) -> Result<(u8, String)> {
    let s = sym.to_uppercase();
    let (m, code) = s.split_at(2.min(s.len()));
    let market = match m {
        "SZ" => 0,
        "SH" => 1,
        "BJ" => 2,
        _ => anyhow::bail!("symbol must look like SH600000 / SZ000001, got {sym}"),
    };
    anyhow::ensure!(code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()), "bad code in {sym}");
    Ok((market, code.to_string()))
}

#[cfg(feature = "tdx")]
#[async_trait]
impl MarketDataSource for RstdxSource {
    fn name(&self) -> &'static str {
        "rstdx"
    }
    async fn daily_bars(&self, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta)> {
        use rstdx::TdxClient;
        let (market, code) = parse_ashare_symbol(symbol)?;
        let timeout = std::time::Duration::from_secs(15);
        let (mut client, server) = match &self.server {
            Some(addr) => (tokio::time::timeout(timeout, TdxClient::connect(addr)).await.context("TDX connect timeout")??, addr.clone()),
            None => (tokio::time::timeout(timeout * 4, TdxClient::connect_auto()).await.context("TDX auto-connect timeout")??, "auto".into()),
        };
        const KLINE_DAILY: u16 = 4;
        let mut all: Vec<Bar> = vec![];
        let mut start: u16 = 0;
        while all.len() < count {
            let n = (count - all.len()).min(800) as u16;
            let page = tokio::time::timeout(timeout, client.get_security_bars(KLINE_DAILY, market, &code, start, n)).await.context("TDX request timeout")??;
            if page.is_empty() {
                break;
            }
            let got = page.len();
            let mut bars: Vec<Bar> = page.into_iter().map(|k| Bar {
                date: format!("{:04}-{:02}-{:02}", k.year, k.month, k.day), open: k.open, high: k.high, low: k.low, close: k.close, volume: k.volume,
            }).collect();
            bars.append(&mut all);
            all = bars;
            start += got as u16;
            if got < n as usize {
                break;
            }
        }
        let _ = client.close().await;
        all.sort_by(|a, b| a.date.cmp(&b.date));
        all.dedup_by(|a, b| a.date == b.date);
        anyhow::ensure!(!all.is_empty(), "TDX returned no bars for {symbol}");
        Ok((all.clone(), DataMeta {
            symbol: symbol.to_uppercase(), source: "rstdx (TDX protocol)".into(), label: "REAL A-SHARE DAILY K-LINE".into(),
            fetched_at: chrono::Utc::now().to_rfc3339(), start: all[0].date.clone(), end: all.last().unwrap().date.clone(),
            bars: all.len(), adjustment: "none (unadjusted TDX prices; splits/dividends NOT adjusted)".into(),
            server: Some(server), extra: serde_json::json!({"rstdx_rev": "77629df (574cd53 + local import fix)", "market": market, "code": code}),
        }))
    }
}

pub fn write_cache(dir: &Path, bars: &[Bar], meta: &DataMeta) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut csv = String::from("date,open,high,low,close,volume\n");
    for b in bars {
        csv.push_str(&format!("{},{},{},{},{},{}\n", b.date, b.open, b.high, b.low, b.close, b.volume));
    }
    let p = dir.join(format!("{}.csv", meta.symbol));
    std::fs::write(&p, csv)?;
    std::fs::write(dir.join(format!("{}.meta.json", meta.symbol)), serde_json::to_string_pretty(meta)?)?;
    Ok(p)
}

pub fn read_meta(dir: &Path, symbol: &str) -> Result<DataMeta> {
    let p = dir.join(format!("{symbol}.meta.json"));
    Ok(serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| format!("no cached data for {symbol}; run `cmsq data fetch {symbol}`"))?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn sample_roundtrip() {
        let (bars, meta) = SampleSource.daily_bars("SAMPLE_TREND", 5000).await.unwrap();
        assert!(bars.len() > 1000);
        assert!(meta.label.contains("SYNTHETIC"));
        let d = tempfile::tempdir().unwrap();
        let p = write_cache(d.path(), &bars, &meta).unwrap();
        assert_eq!(parse_csv(&std::fs::read_to_string(p).unwrap()).unwrap().len(), bars.len());
    }
    #[test]
    fn symbols() {
        assert_eq!(parse_ashare_symbol("sh600000").unwrap(), (1, "600000".into()));
        assert_eq!(parse_ashare_symbol("SZ000001").unwrap().0, 0);
        assert!(parse_ashare_symbol("600000").is_err());
        assert!(parse_ashare_symbol("SH../../x").is_err());
    }
}
