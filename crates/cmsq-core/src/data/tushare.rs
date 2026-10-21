//! Tushare Pro HTTP provider (https://api.tushare.pro). Host-side only.
//! Token: env TUSHARE_TOKEN, else `TUSHARE_TOKEN=` in `<secrets dir>/secrets.env`,
//! `$CMSQ_TUSHARE_ENV_FILE`, or `~/.config/cmsq/secrets.env`.
//! The token is never logged, written to meta, audit or errors.
use super::ratelimit::{backoff, TokenBucket};
use crate::market::{Bar, DataMeta, MarketDataSource};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

pub const ENDPOINT: &str = "https://api.tushare.pro";
/// 5000-point tier allows 500 calls/min/endpoint; keep ~20% headroom.
pub const DEFAULT_RPM: f64 = 400.0;

pub struct Tushare {
    token: String,
    http: reqwest::Client,
    buckets: Mutex<HashMap<String, TokenBucket>>,
    rpm: f64,
    pub calls: std::sync::atomic::AtomicUsize,
    pub limit_hits: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for Tushare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tushare")
            .field("token", &"<redacted>")
            .field("rpm", &self.rpm)
            .finish()
    }
}

pub fn load_token(secrets_dir: &Path) -> Option<String> {
    if let Ok(t) = std::env::var("TUSHARE_TOKEN") {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    let s = std::fs::read_to_string(secrets_dir.join("secrets.env"))
        .or_else(|_| {
            std::fs::read_to_string(std::env::var("CMSQ_TUSHARE_ENV_FILE").unwrap_or_default())
        })
        .or_else(|_| {
            std::fs::read_to_string(
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join(".config/cmsq/secrets.env"),
            )
        })
        .ok()?;
    s.lines().find_map(|l| {
        let l = l.trim().trim_start_matches("export ").trim();
        let v = l
            .strip_prefix("TUSHARE_TOKEN")?
            .trim_start()
            .strip_prefix('=')?;
        let v = v.trim().trim_matches('"').trim_matches('\'');
        (!v.is_empty()).then(|| v.to_string())
    })
}

#[derive(Debug, PartialEq)]
pub enum LimitKind {
    PerMinute(u32),
    PerDay(u32),
}

/// Classify Tushare's rate-limit messages, e.g.
/// "抱歉，您每分钟最多访问该接口500次，权限的具体详情访问：…" / "抱歉，您每天最多访问该接口8000次".
pub fn parse_limit(msg: &str) -> Option<LimitKind> {
    let num = |key: &str| {
        msg.split(key).nth(1).and_then(|r| {
            r.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
    };
    if msg.contains("每分钟") {
        return Some(LimitKind::PerMinute(num("访问该接口").unwrap_or(50)));
    }
    if msg.contains("每天") || msg.contains("每小时") {
        return Some(LimitKind::PerDay(num("访问该接口").unwrap_or(0)));
    }
    None
}

/// "600519.SH" ⇄ "SH600519"
pub fn ts_code(sym: &str) -> Result<String> {
    let (m, code) = crate::market::parse_ashare_symbol(sym)?;
    Ok(format!("{code}.{}", ["SZ", "SH", "BJ"][m as usize]))
}

fn ymd(d: &str) -> String {
    format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8])
}

impl Tushare {
    pub fn new(token: String) -> Result<Self> {
        let rpm = std::env::var("CMSQ_TUSHARE_RPM")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RPM);
        Ok(Self {
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            buckets: Mutex::new(HashMap::new()),
            rpm,
            calls: 0.into(),
            limit_hits: 0.into(),
        })
    }
    pub fn from_secrets(dir: &Path) -> Result<Self> {
        Self::new(
            load_token(dir)
                .context("TUSHARE_TOKEN not set (env or secrets.env in the secrets dir)")?,
        )
    }
    pub fn rate(&self, api: &str) -> f64 {
        self.buckets
            .lock()
            .unwrap()
            .get(api)
            .map(|b| b.per_min())
            .unwrap_or(self.rpm)
    }

    /// One API call → (fields, items). Rate-limited per endpoint, retries with backoff.
    pub async fn query(
        &self,
        api: &str,
        params: Value,
        fields: &str,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut attempt = 0u32;
        loop {
            loop {
                let w = self
                    .buckets
                    .lock()
                    .unwrap()
                    .entry(api.into())
                    .or_insert_with(|| TokenBucket::new(self.rpm))
                    .take();
                if w.is_zero() {
                    break;
                }
                tokio::time::sleep(w).await;
            }
            // shared budget across processes when the optional Redis layer is on
            while let Some(w) = crate::hot::global().take_token(&format!("tushare:{api}"), self.rpm)
            {
                if w.is_zero() {
                    break;
                }
                tokio::time::sleep(w).await;
            }
            self.calls.fetch_add(1, Relaxed);
            let body =
                json!({"api_name": api, "token": self.token, "params": params, "fields": fields});
            let resp = self.http.post(ENDPOINT).json(&body).send().await;
            let v: Value = match resp {
                Ok(r) if r.status().is_success() => r
                    .json()
                    .await
                    .map_err(|e| anyhow::anyhow!("tushare {api}: bad json: {}", e.without_url()))?,
                Ok(r) if attempt < 4 => {
                    tracing_warn(&format!("tushare {api}: http {}", r.status()));
                    attempt += 1;
                    tokio::time::sleep(backoff(Duration::from_secs(2), attempt)).await;
                    continue;
                }
                Ok(r) => anyhow::bail!("tushare {api}: http {}", r.status()),
                Err(e) if attempt < 4 => {
                    tracing_warn(&format!("tushare {api}: {}", e.without_url()));
                    attempt += 1;
                    tokio::time::sleep(backoff(Duration::from_secs(2), attempt)).await;
                    continue;
                }
                Err(e) => anyhow::bail!("tushare {api}: {}", e.without_url()),
            };
            let code = v["code"].as_i64().unwrap_or(-1);
            if code == 0 {
                let d = &v["data"];
                let f = d["fields"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let items = d["items"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|r| r.as_array().cloned()).collect())
                    .unwrap_or_default();
                return Ok((f, items));
            }
            let msg = v["msg"]
                .as_str()
                .unwrap_or("")
                .replace(&self.token, "<redacted>");
            match parse_limit(&msg) {
                Some(LimitKind::PerMinute(n)) if attempt < 5 => {
                    self.limit_hits.fetch_add(1, Relaxed);
                    // the server told us the real tier: adopt 80% of it
                    {
                        let mut g = self.buckets.lock().unwrap();
                        let b = g
                            .entry(api.into())
                            .or_insert_with(|| TokenBucket::new(self.rpm));
                        b.set_rate((n as f64 * 0.8).min(b.per_min()));
                        b.drain();
                    }
                    attempt += 1;
                    tracing_warn(&format!(
                        "tushare {api}: per-minute limit {n} hit, backing off"
                    ));
                    tokio::time::sleep(backoff(Duration::from_secs(8), attempt.min(3))).await;
                }
                Some(LimitKind::PerDay(n)) => {
                    anyhow::bail!("tushare {api}: daily quota exhausted ({n}/day): {msg}")
                }
                _ => anyhow::bail!("tushare {api}: code {code}: {msg}"),
            }
        }
    }

    /// Exchange-published daily price limits: `stk_limit` (A-share) or `ft_limit` (futures).
    /// Returns (YYYY-MM-DD, up, down), ascending. `ts_code` like 600519.SH / RB2601.SHF.
    pub async fn price_limits(
        &self,
        ts_code: &str,
        futures: bool,
        start: &str,
        end: &str,
    ) -> Result<Vec<(String, f64, f64)>> {
        let api = if futures { "ft_limit" } else { "stk_limit" };
        let (cols, rows) = self
            .query(
                api,
                json!({"ts_code": ts_code, "start_date": start.replace('-', ""), "end_date": end.replace('-', "")}),
                "ts_code,trade_date,up_limit,down_limit",
            )
            .await?;
        let ix = |n: &str| cols.iter().position(|c| c == n);
        let (d, u, l) = (
            ix("trade_date").context("trade_date")?,
            ix("up_limit").context("up_limit")?,
            ix("down_limit").context("down_limit")?,
        );
        let num = |v: &Value| v.as_f64().or_else(|| v.as_str()?.parse().ok());
        let mut out: Vec<(String, f64, f64)> = rows
            .iter()
            .filter_map(|r| {
                let day = r.get(d)?.as_str()?;
                let (up, dn) = (num(r.get(u)?)?, num(r.get(l)?)?);
                (day.len() == 8 && up.is_finite() && dn.is_finite() && up > dn && dn > 0.0)
                    .then(|| (ymd(day), up, dn))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        Ok(out)
    }

    pub async fn trade_cal(&self, start: &str, end: &str) -> Result<Vec<(String, bool)>> {
        self.trade_cal_for("SSE", start, end).await
    }

    /// `exchange`: SSE, SZSE, CFFEX, SHFE, CZCE, DCE, INE, GFEX (tushare codes).
    pub async fn trade_cal_for(
        &self,
        exchange: &str,
        start: &str,
        end: &str,
    ) -> Result<Vec<(String, bool)>> {
        let (f, items) = self.query("trade_cal", json!({"exchange": exchange, "start_date": start.replace('-', ""), "end_date": end.replace('-', "")}), "cal_date,is_open").await?;
        let (ci, oi) = (idx(&f, "cal_date")?, idx(&f, "is_open")?);
        let mut v: Vec<(String, bool)> = items
            .iter()
            .filter_map(|r| {
                Some((
                    ymd(r[ci].as_str()?),
                    r[oi].as_i64().or_else(|| r[oi].as_str()?.parse().ok())? == 1,
                ))
            })
            .collect();
        v.sort();
        Ok(v)
    }

    async fn paged(
        &self,
        api: &str,
        ts: &str,
        start: &str,
        end: &str,
        fields: &str,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let mut all = vec![];
        let mut fl = vec![];
        let mut offset = 0;
        loop {
            let (f, items) = self.query(api, json!({"ts_code": ts, "start_date": start, "end_date": end, "limit": 5000, "offset": offset}), fields).await?;
            let n = items.len();
            fl = f;
            all.extend(items);
            offset += n;
            if n < 5000 {
                break;
            }
        }
        Ok((fl, all))
    }
}

impl Tushare {
    /// Main-contract continuous series for `FUT_<PRODUCT>` from fut_basic +
    /// fut_daily of every contract alive in the window, rolled by
    /// [`crate::futures::build_main_series`] (OI lead confirmed 3 closes).
    pub async fn futures_main(&self, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta)> {
        use crate::futures::{build_main_series, spec_for, FutBar};
        let spec = spec_for(symbol).with_context(|| {
            format!(
                "unknown futures product in {symbol} (known: {:?})",
                crate::futures::products()
            )
        })?;
        let today = chrono::Local::now().date_naive();
        let start = (today
            - chrono::Duration::days(((count.min(20_000) as f64) * 1.5) as i64 + 60))
        .format("%Y%m%d")
        .to_string();
        let end = today.format("%Y%m%d").to_string();
        let (bf, basic) = self
            .query(
                "fut_basic",
                json!({"exchange": spec.exchange, "fut_type": "1"}),
                "ts_code,symbol,fut_code,multiplier,list_date,delist_date",
            )
            .await?;
        let (ti, fi, li, di, mi) = (
            idx(&bf, "ts_code")?,
            idx(&bf, "fut_code")?,
            idx(&bf, "list_date")?,
            idx(&bf, "delist_date")?,
            idx(&bf, "multiplier")?,
        );
        let num = |v: &Value| v.as_f64().or_else(|| v.as_str()?.parse().ok());
        let mut contracts: Vec<(String, String, String)> = basic
            .iter()
            .filter(|r| {
                r[fi]
                    .as_str()
                    .is_some_and(|f| f.eq_ignore_ascii_case(&spec.product))
            })
            .filter_map(|r| {
                Some((
                    r[ti].as_str()?.to_string(),
                    r[li].as_str().unwrap_or("").to_string(),
                    r[di].as_str().unwrap_or("99991231").to_string(),
                ))
            })
            .filter(|(_, l, d)| d.as_str() >= start.as_str() && l.as_str() <= end.as_str())
            .collect();
        anyhow::ensure!(
            !contracts.is_empty(),
            "fut_basic lists no {} contracts in the window",
            spec.product
        );
        let mult_seen: Vec<f64> = basic
            .iter()
            .filter(|r| {
                r[fi]
                    .as_str()
                    .is_some_and(|f| f.eq_ignore_ascii_case(&spec.product))
            })
            .filter_map(|r| num(&r[mi]))
            .collect();
        contracts.sort();
        let mut all = vec![];
        for (ts, _, _) in &contracts {
            let (f, items) = self
                .paged(
                    "fut_daily",
                    ts,
                    &start,
                    &end,
                    "ts_code,trade_date,open,high,low,close,settle,vol,oi",
                )
                .await?;
            let (d, o, h, l, c, st, v, oi) = (
                idx(&f, "trade_date")?,
                idx(&f, "open")?,
                idx(&f, "high")?,
                idx(&f, "low")?,
                idx(&f, "close")?,
                idx(&f, "settle")?,
                idx(&f, "vol")?,
                idx(&f, "oi")?,
            );
            for r in &items {
                let (Some(dt), Some(cl)) = (r[d].as_str(), num(&r[c])) else {
                    continue;
                };
                let op = num(&r[o]).unwrap_or(cl);
                all.push(FutBar {
                    contract: ts.split('.').next().unwrap_or(ts).to_string(),
                    date: ymd(dt),
                    open: if op > 0.0 { op } else { cl },
                    high: num(&r[h]).unwrap_or(cl),
                    low: num(&r[l]).unwrap_or(cl),
                    close: cl,
                    settle: num(&r[st]),
                    volume: num(&r[v]).unwrap_or(0.0),
                    oi: num(&r[oi]).unwrap_or(0.0),
                });
            }
        }
        let (mut bars, rolls, held) = build_main_series(&all, 3);
        anyhow::ensure!(bars.len() > 10, "too few main-contract bars for {symbol}");
        let mut held = held;
        if bars.len() > count {
            let cut = bars.len() - count;
            bars.drain(..cut);
            held.drain(..cut);
        }
        // contiguous held-contract segments (used for per-contract cross-validation)
        let mut segs: Vec<(String, String, String)> = vec![];
        for (b, c) in bars.iter().zip(&held) {
            match segs.last_mut() {
                Some(s) if &s.0 == c => s.2 = b.date.clone(),
                _ => segs.push((c.clone(), b.date.clone(), b.date.clone())),
            }
        }
        let rolls: Vec<_> = rolls
            .into_iter()
            .filter(|r| r.date >= bars[0].date)
            .collect();
        Ok((bars.clone(), DataMeta {
            symbol: symbol.to_uppercase(), source: "tushare".into(), label: format!("REAL CN FUTURES DAILY main-contract {} (tushare fut_daily)", spec.product),
            fetched_at: chrono::Utc::now().to_rfc3339(), start: bars[0].date.clone(), end: bars.last().unwrap().date.clone(), bars: bars.len(),
            adjustment: "adj_factor = main-contract roll ratio (raw prices of the held contract; ratio continuity on roll days)".into(), server: Some(ENDPOINT.into()),
            extra: json!({"market": "cn_futures", "spec": spec, "rolls": rolls, "segments": segs, "contracts_fetched": contracts.len(),
                "fut_basic_multiplier": mult_seen.first(), "roll_rule": "largest OI among later contracts, confirmed 3 consecutive closes, effective next trading day; never rolls back"}),
        }))
    }
}

fn idx(f: &[String], k: &str) -> Result<usize> {
    f.iter()
        .position(|x| x == k)
        .with_context(|| format!("tushare response missing field {k}"))
}

fn tracing_warn(m: &str) {
    eprintln!("[data] {m}");
}

#[async_trait]
impl MarketDataSource for Tushare {
    fn name(&self) -> &'static str {
        "tushare"
    }
    async fn daily_bars(&self, symbol: &str, count: usize) -> Result<(Vec<Bar>, DataMeta)> {
        if crate::futures::is_futures(symbol) {
            return self.futures_main(symbol, count).await;
        }
        let ts = ts_code(symbol)?;
        let today = chrono::Local::now().date_naive();
        let span = ((count.min(20_000) as f64) * 1.5) as i64 + 30;
        let cal = self
            .trade_cal(
                &(today - chrono::Duration::days(span))
                    .format("%Y%m%d")
                    .to_string(),
                &today.format("%Y%m%d").to_string(),
            )
            .await?;
        let open: Vec<&String> = cal.iter().filter(|c| c.1).map(|c| &c.0).collect();
        anyhow::ensure!(!open.is_empty(), "empty trade calendar");
        let start = open[open.len().saturating_sub(count)].replace('-', "");
        let end = today.format("%Y%m%d").to_string();
        let (f, items) = self
            .paged(
                "daily",
                &ts,
                &start,
                &end,
                "trade_date,open,high,low,close,vol",
            )
            .await?;
        let (af, aitems) = self
            .paged("adj_factor", &ts, &start, &end, "trade_date,adj_factor")
            .await?;
        let (bf, binfo) = self
            .query(
                "stock_basic",
                json!({"ts_code": ts}),
                "ts_code,name,list_date,market",
            )
            .await
            .unwrap_or_default();
        let num = |v: &Value| v.as_f64().or_else(|| v.as_str()?.parse().ok());
        let adj: HashMap<String, f64> = {
            let (d, a) = (idx(&af, "trade_date")?, idx(&af, "adj_factor")?);
            aitems
                .iter()
                .filter_map(|r| Some((r[d].as_str()?.to_string(), num(&r[a])?)))
                .collect()
        };
        let (d, o, h, l, c, vv) = (
            idx(&f, "trade_date")?,
            idx(&f, "open")?,
            idx(&f, "high")?,
            idx(&f, "low")?,
            idx(&f, "close")?,
            idx(&f, "vol")?,
        );
        let mut raw: HashMap<String, Bar> = HashMap::new();
        for r in &items {
            let Some(dt) = r[d].as_str() else { continue };
            let b = Bar {
                date: ymd(dt),
                open: num(&r[o]).unwrap_or(f64::NAN),
                high: num(&r[h]).unwrap_or(f64::NAN),
                low: num(&r[l]).unwrap_or(f64::NAN),
                close: num(&r[c]).unwrap_or(f64::NAN),
                volume: num(&r[vv]).unwrap_or(0.0) * 100.0,
                adj_factor: adj.get(dt).copied(),
            };
            raw.insert(b.date.clone(), b);
        }
        anyhow::ensure!(!raw.is_empty(), "tushare returned no daily bars for {ts}");
        // walk the exchange calendar: open days without a bar after listing = suspended (volume 0 at previous close)
        let mut bars: Vec<Bar> = vec![];
        let mut suspended = vec![];
        let first = raw.keys().min().unwrap().clone();
        for day in open
            .iter()
            .filter(|d| d.replace('-', "") >= start && ***d >= first)
        {
            match raw.remove(*day) {
                Some(b) => bars.push(b),
                None => {
                    if let Some(p) = bars.last().cloned() {
                        suspended.push((*day).clone());
                        let af = adj.get(&day.replace('-', "")).copied().or(p.adj_factor);
                        bars.push(Bar {
                            date: (*day).clone(),
                            open: p.close,
                            high: p.close,
                            low: p.close,
                            close: p.close,
                            volume: 0.0,
                            adj_factor: af,
                        });
                    }
                }
            }
        }
        let off_calendar: Vec<String> = raw.into_keys().collect();
        // forward-fill missing adj factors (none expected); a bar without a factor makes the series unadjusted
        let missing_adj = bars.iter().filter(|b| b.adj_factor.is_none()).count();
        let name = binfo
            .first()
            .and_then(|r| r.get(idx(&bf, "name").ok()?)?.as_str().map(String::from))
            .unwrap_or_default();
        let list_date = binfo.first().and_then(|r| {
            r.get(idx(&bf, "list_date").ok()?)?
                .as_str()
                .filter(|d| d.len() == 8)
                .map(ymd)
        });
        // ST history from name changes (ST / *ST periods drive the 5% band)
        let st_periods: Vec<(String, String)> = match self
            .query(
                "namechange",
                json!({"ts_code": ts}),
                "name,start_date,end_date",
            )
            .await
        {
            Ok((nf, rows)) => {
                let (ni, si, ei) = (
                    idx(&nf, "name")?,
                    idx(&nf, "start_date")?,
                    idx(&nf, "end_date")?,
                );
                let mut v: Vec<(String, String)> = rows
                    .iter()
                    .filter(|r| {
                        r[ni]
                            .as_str()
                            .is_some_and(|n| n.to_uppercase().contains("ST"))
                    })
                    .filter_map(|r| {
                        Some((
                            ymd(r[si].as_str()?),
                            r[ei]
                                .as_str()
                                .filter(|d| d.len() == 8)
                                .map(ymd)
                                .unwrap_or_else(|| "9999-12-31".into()),
                        ))
                    })
                    .collect();
                v.sort();
                v
            }
            Err(e) => {
                tracing_warn(&format!("tushare namechange unavailable ({e:#}); ST periods taken from the current name only"));
                if name.to_uppercase().contains("ST") {
                    vec![("0000-01-01".into(), "9999-12-31".into())]
                } else {
                    vec![]
                }
            }
        };
        let is_st = name.to_uppercase().contains("ST");
        anyhow::ensure!(!bars.is_empty(), "no bars");
        Ok((
            bars.clone(),
            DataMeta {
                symbol: symbol.to_uppercase(),
                source: "tushare".into(),
                label: "REAL A-SHARE DAILY (tushare pro)".into(),
                fetched_at: chrono::Utc::now().to_rfc3339(),
                start: bars[0].date.clone(),
                end: bars.last().unwrap().date.clone(),
                bars: bars.len(),
                adjustment: if missing_adj == 0 {
                    "adj_factor (tushare adj_factor; container sees hfq = raw×factor)".into()
                } else {
                    format!("none ({missing_adj} bars missing adj_factor)")
                },
                server: Some(ENDPOINT.into()),
                extra: json!({"ts_code": ts, "name": name, "is_st": is_st, "st_periods": st_periods, "list_date": list_date,
                "suspended_days_filled": suspended.len(), "suspended_sample": suspended.iter().take(10).collect::<Vec<_>>(),
                "off_calendar_bars_dropped": off_calendar, "volume_unit": "shares (tushare vol 手 ×100)", "rate_per_min": self.rate("daily")}),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_and_codes() {
        assert_eq!(
            parse_limit(
                "抱歉，您每分钟最多访问该接口500次，权限的具体详情访问：https://tushare.pro"
            ),
            Some(LimitKind::PerMinute(500))
        );
        assert_eq!(
            parse_limit("抱歉，您每天最多访问该接口8000次"),
            Some(LimitKind::PerDay(8000))
        );
        assert_eq!(parse_limit("您的token不对，请确认。"), None);
        assert_eq!(ts_code("SH600519").unwrap(), "600519.SH");
        assert_eq!(ts_code("sz300750").unwrap(), "300750.SZ");
    }
    #[test]
    fn token_from_file_never_debug_printed() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("secrets.env"),
            "# c\nexport TUSHARE_TOKEN=\"abc123\"\n",
        )
        .unwrap();
        if std::env::var("TUSHARE_TOKEN").is_err() {
            assert_eq!(load_token(d.path()).as_deref(), Some("abc123"));
        }
        let t = Tushare::new("abc123".into()).unwrap();
        assert!(!format!("{t:?}").contains("abc123"));
    }
}
