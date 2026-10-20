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
    /// Cumulative backward-adjustment factor (hfq-style: adjusted = raw * factor).
    /// When present the trusted scorer uses it for returns; raw prices stay the
    /// basis for limit-up/down checks and lot sizing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adj_factor: Option<f64>,
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
    ("SAMPLE_REGIME", include_str!("../../../fixtures/market/SAMPLE_REGIME.csv"), include_str!("../../../fixtures/market/SAMPLE_REGIME.meta.json")),
];

impl SampleSource {
    pub fn symbols() -> Vec<&'static str> {
        SAMPLES.iter().map(|s| s.0).collect()
    }
}

/// Parse `date,open,high,low,close,volume[,adj_factor]`. Validates dates are
/// strictly increasing YYYY-MM-DD and prices are finite and positive.
pub fn parse_csv(csv: &str) -> Result<Vec<Bar>> {
    let mut lines = csv.lines();
    let header: Vec<String> = lines.next().unwrap_or("").split(',').map(|h| h.trim().to_lowercase()).collect();
    anyhow::ensure!(header.len() >= 6 && header[..6] == ["date", "open", "high", "low", "close", "volume"], "csv header must start with date,open,high,low,close,volume");
    let has_adj = header.get(6).is_some_and(|h| h == "adj_factor");
    let mut out: Vec<Bar> = vec![];
    for (i, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        anyhow::ensure!(f.len() >= 6, "bad csv line {}", i + 2);
        anyhow::ensure!(chrono::NaiveDate::parse_from_str(f[0], "%Y-%m-%d").is_ok(), "bad date on line {}: {}", i + 2, f[0]);
        let b = Bar { date: f[0].into(), open: f[1].parse()?, high: f[2].parse()?, low: f[3].parse()?, close: f[4].parse()?, volume: f[5].parse()?,
            adj_factor: if has_adj { Some(f.get(6).context("missing adj_factor")?.parse()?) } else { None } };
        anyhow::ensure!([b.open, b.high, b.low, b.close].iter().all(|x| x.is_finite() && *x > 0.0) && b.volume.is_finite() && b.volume >= 0.0,
            "non-finite / non-positive price on line {}", i + 2);
        anyhow::ensure!(b.adj_factor.is_none_or(|a| a.is_finite() && a > 0.0), "bad adj_factor on line {}", i + 2);
        if let Some(p) = out.last() {
            anyhow::ensure!(b.date > p.date, "dates must be strictly increasing (line {})", i + 2);
        }
        out.push(b);
    }
    Ok(out)
}

pub fn to_csv(bars: &[Bar], with_adj: bool) -> String {
    let with_adj = with_adj && bars.iter().all(|b| b.adj_factor.is_some());
    let mut csv = String::from(if with_adj { "date,open,high,low,close,volume,adj_factor\n" } else { "date,open,high,low,close,volume\n" });
    for b in bars {
        csv.push_str(&format!("{},{},{},{},{},{}", b.date, b.open, b.high, b.low, b.close, b.volume));
        if with_adj {
            csv.push_str(&format!(",{}", b.adj_factor.unwrap_or(1.0)));
        }
        csv.push('\n');
    }
    csv
}

pub fn read_bars(dir: &Path, symbol: &str) -> Result<Vec<Bar>> {
    parse_csv(&std::fs::read_to_string(dir.join(format!("{symbol}.csv"))).with_context(|| format!("no cached data for {symbol}"))?)
}

/// Is the series usable for return computation without corporate-action gaps?
pub fn is_adjusted(meta: &DataMeta, bars: &[Bar]) -> bool {
    let a = meta.adjustment.to_lowercase();
    // a price label alone is not enough: lots / fees / limits need RAW prices, so
    // only raw prices + an adj_factor column (or the bundled synthetic samples) count
    let _ = a;
    (meta.source == "sample" && meta.symbol.starts_with("SAMPLE_")) || (!bars.is_empty() && bars.iter().all(|b| b.adj_factor.is_some()))
}

/// User-supplied CSV (e.g. exported adjusted data). `adjustment` must describe it
/// truthfully: "adj_factor" (column present), "qfq", "hfq" or "none".
pub struct CsvSource {
    pub path: std::path::PathBuf,
    pub adjustment: String,
}

#[async_trait]
impl MarketDataSource for CsvSource {
    fn name(&self) -> &'static str {
        "csv"
    }
    async fn daily_bars(&self, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta)> {
        let md = std::fs::symlink_metadata(&self.path)?;
        anyhow::ensure!(md.file_type().is_file() && md.len() < 64 * 1024 * 1024, "csv must be a regular file < 64 MiB");
        let mut bars = parse_csv(&std::fs::read_to_string(&self.path)?)?;
        anyhow::ensure!(!bars.is_empty(), "empty csv");
        if bars.len() > count {
            bars.drain(..bars.len() - count);
        }
        let adj = if bars.iter().all(|b| b.adj_factor.is_some()) { "adj_factor (column)".to_string() } else { self.adjustment.clone() };
        Ok((bars.clone(), DataMeta { symbol: symbol.to_uppercase(), source: "csv".into(), label: format!("USER CSV {}", self.path.display()),
            fetched_at: chrono::Utc::now().to_rfc3339(), start: bars[0].date.clone(), end: bars.last().unwrap().date.clone(), bars: bars.len(),
            adjustment: adj, server: None, extra: serde_json::json!({}) }))
    }
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
                date: format!("{:04}-{:02}-{:02}", k.year, k.month, k.day), open: k.open, high: k.high, low: k.low, close: k.close, volume: k.volume, adj_factor: None,
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
            server: Some(server), extra: serde_json::json!({"rstdx_rev": "vendored third_party/rstdx (574cd53 + 77629df + 009b78c)", "market": market, "code": code}),
        }))
    }
}

/// Write the cache csv + meta, plus an immutable content-addressed snapshot
/// `data/snapshots/<sha256>.csv` (mode 0444). The sha is stored in meta.extra.
pub fn write_cache(dir: &Path, bars: &[Bar], meta: &DataMeta) -> Result<std::path::PathBuf> {
    use sha2::Digest;
    std::fs::create_dir_all(dir.join("snapshots"))?;
    anyhow::ensure!(meta.symbol.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !meta.symbol.is_empty(), "bad symbol");
    let csv = to_csv(bars, true);
    let sha = hex::encode(sha2::Sha256::digest(csv.as_bytes()));
    let snap = dir.join("snapshots").join(format!("{sha}.csv"));
    if !snap.exists() {
        std::fs::write(&snap, &csv)?;
        set_readonly(&snap)?;
    }
    let mut meta = meta.clone();
    if !meta.extra.is_object() {
        meta.extra = serde_json::json!({"original": meta.extra});
    }
    meta.extra["snapshot_sha256"] = sha.into();
    let p = dir.join(format!("{}.csv", meta.symbol));
    let _ = std::fs::remove_file(&p);
    std::fs::write(&p, csv)?;
    set_readonly(&p)?;
    std::fs::write(dir.join(format!("{}.meta.json", meta.symbol)), serde_json::to_string_pretty(&meta)?)?;
    Ok(p)
}

fn set_readonly(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o444))?)
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
        assert_eq!(parse_csv(&std::fs::read_to_string(&p).unwrap()).unwrap().len(), bars.len());
        let m = read_meta(d.path(), "SAMPLE_TREND").unwrap();
        let sha = m.extra["snapshot_sha256"].as_str().unwrap().to_string();
        assert!(d.path().join("snapshots").join(format!("{sha}.csv")).exists());
        write_cache(d.path(), &bars, &meta).unwrap(); // rewrite over read-only cache works
    }
    #[test]
    fn symbols() {
        assert_eq!(parse_ashare_symbol("sh600000").unwrap(), (1, "600000".into()));
        assert_eq!(parse_ashare_symbol("SZ000001").unwrap().0, 0);
        assert!(parse_ashare_symbol("600000").is_err());
        assert!(parse_ashare_symbol("SH../../x").is_err());
    }
}
