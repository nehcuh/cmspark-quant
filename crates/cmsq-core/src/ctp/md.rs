//! Observe-only CTP market data: feed abstraction, trusted-tick validation,
//! one-minute bar aggregation, persistence and audit.
//!
//! There is deliberately no order path here. The live feed (feature `ctp`)
//! wraps ctp2rs `MdApi` only; the order-entry API is never referenced (a unit test
//! greps this module tree to keep it that way).
use super::Tick;
use crate::error::CoreError;
use crate::orchestrator::Orchestrator;
use anyhow::Result;
use chrono::{NaiveDate, NaiveDateTime, Timelike};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

/// openctp 7x24 simulated market-data front (public, no account needed).
pub const OPENCTP_7X24_MD: &str = "tcp://trading.openctp.cn:30011";
/// Anything at or above this is CTP's "no value" sentinel (DBL_MAX) or garbage.
pub(crate) fn inv(s: impl Into<String>) -> anyhow::Error {
    CoreError::Invalid(s.into()).into()
}

const PRICE_CEIL: f64 = 1e11;

#[derive(Debug, Clone)]
pub struct MdConfig {
    pub front: String,
    pub instruments: Vec<String>,
    pub broker_id: String,
    pub duration: Duration,
}

/// A depth-market-data record as delivered by the front, before validation.
#[derive(Debug, Clone, Default)]
pub struct RawTick {
    pub instrument: String,
    /// YYYYMMDD as reported by the exchange (unreliable for night sessions on some exchanges)
    pub trading_day: String,
    /// YYYYMMDD calendar day of the update
    pub action_day: String,
    /// HH:MM:SS
    pub update_time: String,
    pub millis: i32,
    pub last: f64,
    /// cumulative volume for the trading day
    pub volume: f64,
    pub open_interest: f64,
    pub bid1: f64,
    pub ask1: f64,
    pub upper_limit: f64,
    pub lower_limit: f64,
}

pub enum FeedEvent {
    Tick(RawTick),
    /// informational (connected, logged in, subscribed, disconnected...)
    Status(String),
}

/// Source of depth market data. Implementations: [`ReplayFeed`] (mock/offline)
/// and, with feature `ctp`, the ctp2rs MdApi feed.
pub trait MdFeed: Send {
    fn name(&self) -> &str;
    fn start(&mut self, cfg: &MdConfig) -> Result<()>;
    /// Next event, or None once `timeout` elapses without one / the feed ends.
    fn next(&mut self, timeout: Duration) -> Option<FeedEvent>;
    fn finished(&self) -> bool {
        false
    }
}

/// Deterministic replay feed. `demo` produces three minutes of a night-session
/// open (Fri 20:59:58 → 21:02) per instrument, interleaved with malformed
/// records that the validator must reject.
pub struct ReplayFeed {
    items: std::collections::VecDeque<RawTick>,
}

impl ReplayFeed {
    pub fn new(items: Vec<RawTick>) -> Self {
        Self {
            items: items.into(),
        }
    }
    pub fn demo(instruments: &[String]) -> Self {
        let mut v = Vec::new();
        for (k, ins) in instruments.iter().enumerate() {
            let base = 3500.0 + 100.0 * k as f64;
            let mut vol = 0.0;
            let mut push =
                |h: u32, m: u32, s: u32, ms: i32, last: f64, dv: f64, v: &mut Vec<RawTick>| {
                    vol += dv;
                    v.push(RawTick {
                        instrument: ins.clone(),
                        trading_day: "20261019".into(),
                        action_day: "20261016".into(),
                        update_time: format!("{h:02}:{m:02}:{s:02}"),
                        millis: ms,
                        last,
                        volume: vol,
                        open_interest: 1.0e6,
                        bid1: last - 1.0,
                        ask1: last + 1.0,
                        upper_limit: base * 1.07,
                        lower_limit: base * 0.93,
                    });
                };
            push(20, 59, 58, 0, base, 10.0, &mut v); // pre-open auction print
            for i in 0..180u32 {
                let p = base + ((i as f64) * 0.7).sin().mul_add(8.0, 0.0).round();
                push(21, i / 60, i % 60, 500, p, 3.0, &mut v);
            }
            // malformed records
            let mut bad = v.last().cloned().unwrap();
            bad.last = f64::MAX;
            v.push(bad.clone());
            bad.last = f64::NAN;
            v.push(bad.clone());
            bad.last = base * 1.5; // beyond the upper limit
            v.push(bad.clone());
            bad.last = base;
            bad.update_time = "21:00:30".into(); // time goes backwards
            v.push(bad.clone());
            bad.update_time = "21:02:59".into();
            bad.volume -= 100.0; // cumulative volume goes backwards
            v.push(bad.clone());
            bad.volume += 200.0;
            bad.update_time = "25:61:00".into(); // unparsable time
            v.push(bad);
        }
        let mut other = v[0].clone();
        other.instrument = "zz9999".into(); // not subscribed
        v.push(other);
        Self::new(v)
    }
}

impl MdFeed for ReplayFeed {
    fn name(&self) -> &str {
        "replay"
    }
    fn start(&mut self, _cfg: &MdConfig) -> Result<()> {
        Ok(())
    }
    fn next(&mut self, _timeout: Duration) -> Option<FeedEvent> {
        self.items.pop_front().map(FeedEvent::Tick)
    }
    fn finished(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MinuteBar {
    pub instrument: String,
    pub trading_day: String,
    /// bar start, local exchange time
    pub minute: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub ticks: u32,
}

/// Validates ticks and aggregates them into one-minute bars.
#[derive(Default)]
pub struct Aggregator {
    allowed: HashSet<String>,
    last_ts: HashMap<String, NaiveDateTime>,
    last_vol: HashMap<String, f64>,
    cur: HashMap<String, MinuteBar>,
    pub bars: Vec<MinuteBar>,
    pub rejected: BTreeMap<String, u64>,
    /// per-exchange calendars (snapshot or weekday fallback) for the trading-day label
    pub calendars: crate::calendars::FuturesCalendars,
    /// ticks accepted but flagged: e.g. a night-session tick on a holiday eve
    /// (exchanges hold no night session then; simulated 7x24 fronts do)
    pub flagged: BTreeMap<String, u64>,
}

fn clean(x: f64) -> Option<f64> {
    (x.is_finite() && x > 0.0 && x < PRICE_CEIL).then_some(x)
}

impl Aggregator {
    pub fn new(instruments: &[String]) -> Self {
        Self {
            allowed: instruments.iter().cloned().collect(),
            ..Default::default()
        }
    }
    fn reject(&mut self, why: &str) -> Option<Tick> {
        *self.rejected.entry(why.into()).or_default() += 1;
        None
    }
    /// Returns the trusted tick, or None (counted under a reason) when rejected.
    pub fn accept(&mut self, r: &RawTick, source: &str) -> Option<Tick> {
        if !self.allowed.contains(&r.instrument) {
            return self.reject("not_subscribed");
        }
        let Some(last) = clean(r.last) else {
            return self.reject("bad_price");
        };
        if let (Some(lo), Some(hi)) = (clean(r.lower_limit), clean(r.upper_limit)) {
            if last < lo - 1e-9 || last > hi + 1e-9 {
                return self.reject("outside_limits");
            }
        }
        if !r.volume.is_finite() || r.volume < 0.0 {
            return self.reject("bad_volume");
        }
        let day = NaiveDate::parse_from_str(&r.action_day, "%Y%m%d").ok();
        let time = chrono::NaiveTime::parse_from_str(&r.update_time, "%H:%M:%S").ok();
        let (Some(day), Some(time)) = (day, time) else {
            return self.reject("bad_time");
        };
        let ts = day.and_time(time) + chrono::Duration::milliseconds(r.millis.clamp(0, 999) as i64);
        if let Some(p) = self.last_ts.get(&r.instrument) {
            if ts < *p {
                return self.reject("time_backwards");
            }
        }
        if let Some(v) = self.last_vol.get(&r.instrument) {
            if r.volume < *v {
                return self.reject("volume_backwards");
            }
        }
        let dv = r.volume
            - self
                .last_vol
                .get(&r.instrument)
                .copied()
                .unwrap_or(r.volume);
        self.last_ts.insert(r.instrument.clone(), ts);
        self.last_vol.insert(r.instrument.clone(), r.volume);
        let cal = self.calendars.for_instrument(&r.instrument);
        if let Some(ev) = crate::calendars::ExchangeCalendar::night_evening(ts) {
            if cal.cal.is_some() && !cal.has_night_session(ev) {
                *self
                    .flagged
                    .entry("no_night_session_per_calendar".into())
                    .or_default() += 1;
            }
        }
        let td = cal.trading_day(ts).format("%Y-%m-%d").to_string();
        let minute = ts
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(ts)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        match self.cur.get_mut(&r.instrument) {
            Some(b) if b.minute == minute => {
                b.high = b.high.max(last);
                b.low = b.low.min(last);
                b.close = last;
                b.volume += dv;
                b.ticks += 1;
            }
            _ => {
                let nb = MinuteBar {
                    instrument: r.instrument.clone(),
                    trading_day: td.clone(),
                    minute,
                    open: last,
                    high: last,
                    low: last,
                    close: last,
                    volume: dv,
                    ticks: 1,
                };
                if let Some(old) = self.cur.insert(r.instrument.clone(), nb) {
                    self.bars.push(old);
                }
            }
        }
        Some(Tick {
            instrument: r.instrument.clone(),
            ts: format!("{}+08:00", ts.format("%Y-%m-%dT%H:%M:%S%.3f")),
            trading_day: td,
            last,
            volume: r.volume,
            open_interest: clean(r.open_interest),
            bid1: clean(r.bid1),
            ask1: clean(r.ask1),
            source: source.into(),
        })
    }
    /// Close out the partial bars.
    pub fn flush(&mut self) {
        let mut rest: Vec<_> = self.cur.drain().map(|(_, b)| b).collect();
        rest.sort_by(|a, b| (&a.instrument, &a.minute).cmp(&(&b.instrument, &b.minute)));
        self.bars.extend(rest);
    }
}

#[derive(Debug, Serialize)]
pub struct MdReport {
    pub feed: String,
    pub front: String,
    pub ticks: usize,
    pub bars: usize,
    pub rejected: u64,
    pub rejected_by_reason: BTreeMap<String, u64>,
    pub status: Vec<String>,
    pub bars_csv: String,
    /// accepted-but-flagged ticks (e.g. night session the calendar says does not exist)
    pub flagged: BTreeMap<String, u64>,
    /// trading-day calendar used per exchange of the subscribed instruments
    pub calendars: BTreeMap<String, String>,
}

pub async fn run_md(o: &Orchestrator, feed: Box<dyn MdFeed>, cfg: &MdConfig) -> Result<MdReport> {
    tokio::task::block_in_place(|| run_md_blocking(o, feed, cfg))
}

pub fn run_md_blocking(
    o: &Orchestrator,
    mut feed: Box<dyn MdFeed>,
    cfg: &MdConfig,
) -> Result<MdReport> {
    if cfg.instruments.is_empty() {
        return Err(inv("no instruments to subscribe"));
    }
    if let Some(bad) = cfg
        .instruments
        .iter()
        .find(|i| i.len() > 30 || !i.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        return Err(inv(format!("bad instrument id {bad:?}")));
    }
    feed.start(cfg)?;
    let source = format!("ctp:{}", feed.name());
    let mut agg = Aggregator::new(&cfg.instruments);
    agg.calendars = crate::calendars::FuturesCalendars::load(&o.home.data_dir());
    let mut buf: Vec<Tick> = Vec::new();
    let mut stored = 0usize;
    let mut status = Vec::new();
    let end = Instant::now() + cfg.duration;
    while Instant::now() < end && !feed.finished() {
        match feed.next(
            end.saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500)),
        ) {
            Some(FeedEvent::Tick(r)) => {
                if let Some(t) = agg.accept(&r, &source) {
                    buf.push(t);
                }
                if buf.len() >= 500 {
                    stored += o.store.insert_ticks(&buf)?;
                    buf.clear();
                }
            }
            Some(FeedEvent::Status(s)) => status.push(s),
            None => {}
        }
    }
    stored += o.store.insert_ticks(&buf)?;
    agg.flush();
    let dir = o.home.root.join("runs").join(format!(
        "md_{}",
        chrono::Local::now().format("%Y%m%dT%H%M%S%.3f")
    ));
    std::fs::create_dir_all(&dir)?;
    let csv = dir.join("bars_1m.csv");
    let mut s = String::from("instrument,trading_day,minute,open,high,low,close,volume,ticks\n");
    for b in &agg.bars {
        s += &format!(
            "{},{},{},{},{},{},{},{},{}\n",
            b.instrument,
            b.trading_day,
            b.minute,
            b.open,
            b.high,
            b.low,
            b.close,
            b.volume,
            b.ticks
        );
    }
    std::fs::write(&csv, s)?;
    let rep = MdReport {
        feed: feed.name().into(),
        front: cfg.front.clone(),
        ticks: stored,
        bars: agg.bars.len(),
        rejected: agg.rejected.values().sum(),
        rejected_by_reason: agg.rejected.clone(),
        status,
        bars_csv: csv.display().to_string(),
        flagged: agg.flagged.clone(),
        calendars: cfg
            .instruments
            .iter()
            .map(|i| {
                let c = agg.calendars.for_instrument(i);
                (c.exchange.clone(), c.source())
            })
            .collect(),
    };
    o.store.audit("md", "md_session", None, serde_json::json!({"feed": rep.feed, "front": rep.front, "instruments": cfg.instruments,
        "ticks": rep.ticks, "bars": rep.bars, "rejected": rep.rejected_by_reason, "flagged": rep.flagged, "calendars": rep.calendars, "observe_only": true}), true)?;
    Ok(rep)
}

/// TCP reachability of a front such as `tcp://host:port`; returns connect time in ms.
pub async fn probe_front(front: &str, timeout: Duration) -> Result<u128> {
    let addr = front.strip_prefix("tcp://").unwrap_or(front);
    if !addr.contains(':') {
        return Err(inv(format!("front must be tcp://host:port, got {front:?}")));
    }
    let t0 = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => Ok(t0.elapsed().as_millis()),
        Ok(Err(e)) => Err(inv(format!("connect failed: {e}"))),
        Err(_) => Err(inv(format!(
            "connect timed out after {}ms",
            timeout.as_millis()
        ))),
    }
}

/// The live CTP feed (ctp2rs MdApi). Needs `--features ctp` at build time and
/// `CMSQ_CTP_MD_LIB` pointing at a CTP/openctp `thostmduserapi_se.so`.
pub fn live_feed() -> Result<Box<dyn MdFeed>> {
    #[cfg(feature = "ctp")]
    {
        Ok(Box::new(super::live::CtpMdFeed::from_env()?))
    }
    #[cfg(not(feature = "ctp"))]
    {
        Err(inv("this build has no live CTP feed: rebuild with `--features ctp` (cmsq-cli) and set CMSQ_CTP_MD_LIB; use --mock for the replay feed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins() -> Vec<String> {
        vec!["rb2601".into(), "au2612".into()]
    }

    #[test]
    fn demo_feed_rejects_every_malformed_record() {
        let mut f = ReplayFeed::demo(&ins());
        let mut a = Aggregator::new(&ins());
        let mut ok = 0;
        while let Some(FeedEvent::Tick(r)) = f.next(Duration::ZERO) {
            if a.accept(&r, "t").is_some() {
                ok += 1;
            }
        }
        a.flush();
        assert_eq!(ok, 2 * 181);
        for k in [
            "bad_price",
            "outside_limits",
            "time_backwards",
            "volume_backwards",
            "bad_time",
        ] {
            assert!(
                a.rejected.get(k).copied().unwrap_or(0) >= 2,
                "{k}: {:?}",
                a.rejected
            );
        }
        assert_eq!(a.rejected["bad_price"], 4); // DBL_MAX + NaN per instrument
        assert_eq!(a.rejected["not_subscribed"], 1);
        // 20:59 + 21:00..21:02 per instrument
        assert_eq!(a.bars.len(), 8);
        let b = a
            .bars
            .iter()
            .find(|b| b.instrument == "rb2601" && b.minute.ends_with("21:00"))
            .unwrap();
        assert_eq!(b.ticks, 60);
        assert_eq!(b.volume, 180.0);
        // Friday-night ticks belong to Monday's trading day
        assert!(
            a.bars.iter().all(|b| b.trading_day == "2026-10-19"),
            "{:?}",
            a.bars[0]
        );
    }

    #[test]
    fn sentinel_quotes_become_none() {
        let mut a = Aggregator::new(&ins());
        let r = RawTick {
            instrument: "rb2601".into(),
            action_day: "20261020".into(),
            update_time: "10:00:00".into(),
            last: 3500.0,
            volume: 1.0,
            bid1: f64::MAX,
            ask1: 3501.0,
            open_interest: f64::MAX,
            upper_limit: f64::MAX,
            lower_limit: f64::MAX,
            ..Default::default()
        };
        let t = a.accept(&r, "t").unwrap();
        assert_eq!(
            (t.bid1, t.ask1, t.open_interest),
            (None, Some(3501.0), None)
        );
        assert_eq!(t.trading_day, "2026-10-20");
    }

    #[test]
    fn exchange_calendar_labels_holiday_eve_night_ticks() {
        use chrono::Datelike;
        // SHFE snapshot: National Day 2024 (10-01..10-07 closed)
        let mut days = vec![];
        let mut x = NaiveDate::from_ymd_opt(2024, 9, 23).unwrap();
        while x <= NaiveDate::from_ymd_opt(2024, 10, 18).unwrap() {
            let wk = !matches!(x.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun);
            days.push((
                x.format("%Y-%m-%d").to_string(),
                wk && !(x.month() == 10 && x.day() <= 7),
            ));
            x += chrono::Duration::days(1);
        }
        let mut a = Aggregator::new(&ins());
        a.calendars.by_exchange.insert(
            "SHFE".into(),
            crate::calendars::ExchangeCalendar {
                exchange: "SHFE".into(),
                cal: Some(crate::replay::Calendar::from_days(days)),
            },
        );
        let tick = |day: &str, t: &str, v: f64| RawTick {
            instrument: "rb2601".into(),
            action_day: day.into(),
            update_time: t.into(),
            last: 3500.0,
            volume: v,
            ..Default::default()
        };
        let t = a.accept(&tick("20240930", "21:01:00", 1.0), "t").unwrap();
        assert_eq!(
            t.trading_day, "2024-10-08",
            "holiday skipped via the exchange calendar"
        );
        assert_eq!(a.flagged.get("no_night_session_per_calendar"), Some(&1));
        let t = a.accept(&tick("20241008", "21:01:00", 2.0), "t").unwrap();
        assert_eq!(t.trading_day, "2024-10-09");
        assert_eq!(a.flagged.get("no_night_session_per_calendar"), Some(&1));
        // no snapshot for the exchange: weekday fallback, nothing flagged
        let mut b = Aggregator::new(&ins());
        assert_eq!(
            b.accept(&tick("20240930", "21:01:00", 1.0), "t")
                .unwrap()
                .trading_day,
            "2024-10-01"
        );
        assert!(b.flagged.is_empty());
    }

    #[test]
    fn no_trading_api_anywhere_in_ctp_module() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ctp");
        let needle = ["Trader", "Api"].concat();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let s = std::fs::read_to_string(&p).unwrap();
            let hits = s.matches(&needle).count();
            assert_eq!(hits, 0, "{} references the trading API", p.display());
        }
    }

    #[tokio::test]
    async fn probe_rejects_garbage() {
        assert!(probe_front("nonsense", Duration::from_millis(50))
            .await
            .is_err());
    }
}
