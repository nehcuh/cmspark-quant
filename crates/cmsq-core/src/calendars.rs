//! P2: per-exchange trade calendars for China futures (SHFE / DCE / CZCE /
//! CFFEX / INE / GFEX) so night sessions map to the right trading day across
//! holidays, and holiday-eve night sessions (which exchanges cancel) are
//! recognised.
//!
//! Source of truth: tushare `trade_cal` per exchange, fetched by
//! `cmsq data calendar --exchange <EX|futures>` and cached as a snapshot file
//! `data/_calendar_<EX>.txt` (sha256 recorded in the audit log). Nothing is
//! hard-coded: without a snapshot the only fallback is "every weekday is open",
//! and every caller reports which one it used.
use crate::replay::Calendar;
use chrono::{Datelike, Duration, NaiveDate, Weekday};
use std::collections::BTreeMap;
use std::path::Path;

pub fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(b))
}

pub const FUTURES_EXCHANGES: [&str; 6] = ["SHFE", "DCE", "CZCE", "CFFEX", "INE", "GFEX"];

/// Calendar for one exchange, or the weekday fallback.
#[derive(Debug, Clone)]
pub struct ExchangeCalendar {
    pub exchange: String,
    /// None = weekday fallback (no snapshot)
    pub cal: Option<Calendar>,
    /// Set when a snapshot file exists but is malformed: callers must warn,
    /// audit it, and refuse in a real workspace.
    pub invalid: Option<String>,
}

fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

impl ExchangeCalendar {
    pub fn weekdays(exchange: &str) -> Self {
        Self {
            exchange: exchange.into(),
            cal: None,
            invalid: None,
        }
    }
    pub fn path(data_dir: &Path, exchange: &str) -> std::path::PathBuf {
        data_dir.join(format!("_calendar_{exchange}.txt"))
    }
    /// Snapshot if present, weekday fallback otherwise.
    pub fn load(data_dir: &Path, exchange: &str) -> Self {
        let p = Self::path(data_dir, exchange);
        if !p.exists() {
            return Self::weekdays(exchange);
        }
        match Calendar::load(&p) {
            Ok(c) => Self {
                exchange: exchange.into(),
                cal: Some(c),
                invalid: None,
            },
            Err(e) => Self {
                exchange: exchange.into(),
                cal: None,
                invalid: Some(format!("{e:#}")),
            },
        }
    }
    pub fn source(&self) -> String {
        match &self.cal {
            Some(c) => format!(
                "tushare trade_cal {} snapshot {}..{}",
                self.exchange, c.first, c.last
            ),
            None if self.invalid.is_some() => format!(
                "INVALID _calendar_{}.txt ({}); weekday fallback",
                self.exchange,
                self.invalid.as_deref().unwrap_or("")
            ),
            None => format!(
                "weekday fallback (no _calendar_{}.txt; run `cmsq data calendar --exchange {}`)",
                self.exchange, self.exchange
            ),
        }
    }
    /// Malformed snapshots across all exchanges: (exchange, reason).
    /// Real workspaces refuse; DEMO / tests may proceed with a warning.
    pub fn ensure_valid(&self, real_workspace: bool) -> anyhow::Result<()> {
        if let Some(e) = &self.invalid {
            anyhow::ensure!(
                !real_workspace,
                "calendar _calendar_{}.txt is malformed ({e}); refusing in a real workspace — re-run `cmsq data calendar --exchange {}`",
                self.exchange,
                self.exchange
            );
            eprintln!(
                "  ⚠ calendar _calendar_{}.txt is malformed ({e}); using weekday fallback",
                self.exchange
            );
        }
        Ok(())
    }
    /// Outside the snapshot range we fall back to weekdays rather than guess.
    pub fn is_open(&self, d: NaiveDate) -> bool {
        let weekday = !matches!(d.weekday(), Weekday::Sat | Weekday::Sun);
        match &self.cal {
            Some(c) => {
                let s = ymd(d);
                if s < c.first || s > c.last {
                    weekday
                } else {
                    c.is_open(&s)
                }
            }
            None => weekday,
        }
    }
    pub fn next_open(&self, d: NaiveDate) -> NaiveDate {
        let mut x = d;
        for _ in 0..60 {
            x += Duration::days(1);
            if self.is_open(x) {
                return x;
            }
        }
        x
    }
    /// Does the night session on the evening of calendar day `d` exist? Exchanges
    /// hold it only when `d` is a trading day and the next trading day follows
    /// after nothing but a weekend (no night session before a holiday).
    pub fn has_night_session(&self, d: NaiveDate) -> bool {
        if !self.is_open(d) {
            return false;
        }
        let nxt = self.next_open(d);
        let mut x = d + Duration::days(1);
        while x < nxt {
            if !matches!(x.weekday(), Weekday::Sat | Weekday::Sun) {
                return false; // a weekday holiday in between
            }
            x += Duration::days(1);
        }
        true
    }
    /// Trading day of a timestamp, see [`crate::futures::trading_day_for`].
    pub fn trading_day(&self, ts: chrono::NaiveDateTime) -> NaiveDate {
        crate::futures::trading_day_for(ts, |d| self.is_open(d))
    }
    /// Calendar evening a night-session timestamp belongs to (None for day session).
    pub fn night_evening(ts: chrono::NaiveDateTime) -> Option<NaiveDate> {
        use chrono::Timelike;
        let h = ts.time().hour();
        if h >= 20 {
            Some(ts.date())
        } else if h < 3 {
            Some(ts.date() - Duration::days(1))
        } else {
            None
        }
    }
}

/// All futures exchanges, keyed by exchange code.
#[derive(Debug, Clone, Default)]
pub struct FuturesCalendars {
    pub by_exchange: BTreeMap<String, ExchangeCalendar>,
}

impl FuturesCalendars {
    pub fn load(data_dir: &Path) -> Self {
        Self {
            by_exchange: FUTURES_EXCHANGES
                .iter()
                .map(|e| (e.to_string(), ExchangeCalendar::load(data_dir, e)))
                .collect(),
        }
    }
    pub fn invalid(&self) -> Vec<(String, String)> {
        self.by_exchange
            .values()
            .filter_map(|c| c.invalid.clone().map(|e| (c.exchange.clone(), e)))
            .collect()
    }
    pub fn get(&self, exchange: &str) -> ExchangeCalendar {
        self.by_exchange
            .get(exchange)
            .cloned()
            .unwrap_or_else(|| ExchangeCalendar::weekdays(exchange))
    }
    /// Exchange of a CTP instrument id (`rb2510`, `IF2512`, `SR509`) via the contract table.
    pub fn exchange_of_instrument(instrument: &str) -> Option<String> {
        let p: String = instrument
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        crate::futures::spec_for(&format!("FUT_{}", p.to_uppercase())).map(|s| s.exchange)
    }
    pub fn for_instrument(&self, instrument: &str) -> ExchangeCalendar {
        match Self::exchange_of_instrument(instrument) {
            Some(e) => self.get(&e),
            None => ExchangeCalendar::weekdays("?"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }
    fn dt(s: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }
    /// National Day 2024: 10-01..10-07 closed, back on Tue 10-08.
    fn shfe_2024_oct() -> ExchangeCalendar {
        let mut days = vec![];
        let mut x = d("2024-09-23");
        while x <= d("2024-10-18") {
            let wk = !matches!(x.weekday(), Weekday::Sat | Weekday::Sun);
            let hol = x >= d("2024-10-01") && x <= d("2024-10-07");
            days.push((ymd(x), wk && !hol));
            x += Duration::days(1);
        }
        ExchangeCalendar {
            exchange: "SHFE".into(),
            cal: Some(Calendar::from_days(days)),
            invalid: None,
        }
    }

    #[test]
    fn night_session_maps_across_holiday() {
        let c = shfe_2024_oct();
        // Thursday 09-26 night -> Friday 09-27
        assert_eq!(c.trading_day(dt("2024-09-26 21:30:00")), d("2024-09-27"));
        // a (spurious) Monday 09-30 night tick would belong to Tue 10-08, and that night does not exist
        assert_eq!(c.trading_day(dt("2024-09-30 21:30:00")), d("2024-10-08"));
        assert!(!c.has_night_session(d("2024-09-30")));
        // Friday 09-27 night exists (only a weekend before Monday 09-30)
        assert!(c.has_night_session(d("2024-09-27")));
        assert_eq!(c.trading_day(dt("2024-09-28 00:30:00")), d("2024-09-30"));
        // weekday fallback would get it wrong: that is why the snapshot exists
        let w = ExchangeCalendar::weekdays("SHFE");
        assert_eq!(w.trading_day(dt("2024-09-30 21:30:00")), d("2024-10-01"));
        assert!(w.has_night_session(d("2024-09-30")));
        assert!(w.source().contains("fallback") && c.source().contains("snapshot"));
        assert_eq!(
            ExchangeCalendar::night_evening(dt("2024-09-28 00:30:00")),
            Some(d("2024-09-27"))
        );
        assert_eq!(
            ExchangeCalendar::night_evening(dt("2024-09-27 10:00:00")),
            None
        );
    }

    #[test]
    fn instrument_exchange_lookup() {
        assert_eq!(
            FuturesCalendars::exchange_of_instrument("rb2510").as_deref(),
            Some("SHFE")
        );
        assert_eq!(
            FuturesCalendars::exchange_of_instrument("IF2512").as_deref(),
            Some("CFFEX")
        );
        assert_eq!(
            FuturesCalendars::exchange_of_instrument("SR509").as_deref(),
            Some("CZCE")
        );
        assert_eq!(FuturesCalendars::exchange_of_instrument("zz123"), None);
    }

    #[test]
    fn outside_snapshot_range_falls_back_to_weekdays() {
        let c = shfe_2024_oct();
        assert!(c.is_open(d("2025-01-06")) && !c.is_open(d("2025-01-04")));
    }

    #[test]
    fn malformed_snapshot_is_flagged_not_silently_weekdays() {
        let t = tempfile::tempdir().unwrap();
        let w =
            |body: &str| std::fs::write(ExchangeCalendar::path(t.path(), "SHFE"), body).unwrap();
        // missing file: plain fallback, not invalid
        let c = ExchangeCalendar::load(t.path(), "SHFE");
        assert!(c.cal.is_none() && c.invalid.is_none());
        assert!(c.ensure_valid(true).is_ok());
        for bad in [
            "# SHFE test (open days only)\n2026-09-29\n", // no range
            "2026-09-29\n2026-09-30\n",                   // no header
            "# SHFE trade calendar 2026-09-29..2026-10-12 x\n2026-9-30\n", // bad date
            "# SHFE trade calendar 2026-09-29..2026-10-12 x\n2026-11-02\n", // out of range
            "# SHFE trade calendar 2026-10-12..2026-09-29 x\n2026-10-01\n", // reversed
        ] {
            w(bad);
            let c = ExchangeCalendar::load(t.path(), "SHFE");
            assert!(c.cal.is_none() && c.invalid.is_some(), "{bad:?}");
            assert!(c.source().contains("INVALID"));
            assert!(c.ensure_valid(true).is_err(), "real workspace must refuse");
            assert!(c.ensure_valid(false).is_ok(), "demo warns only");
            assert_eq!(FuturesCalendars::load(t.path()).invalid().len(), 1);
        }
        w("# SHFE trade calendar 2026-09-29..2026-10-12 (open days only)\n2026-09-29\n2026-09-30\n");
        let c = ExchangeCalendar::load(t.path(), "SHFE");
        assert!(c.cal.is_some() && c.invalid.is_none());
    }
}
