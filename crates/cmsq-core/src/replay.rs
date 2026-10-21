//! Native bar-by-bar replay (replaces huquant's BarPlayer/Strategy): an exchange
//! trade calendar, a `Strategy` trait that only ever sees bars up to "now", and
//! reference strategies. Host-side, trusted; used for reference runs and for
//! cross-checking the sandboxed runner, never as a substitute for the gate.
use crate::market::Bar;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Calendar {
    pub open: BTreeSet<String>,
    pub first: String,
    pub last: String,
}

impl Calendar {
    pub fn from_days(days: impl IntoIterator<Item = (String, bool)>) -> Self {
        let all: Vec<(String, bool)> = days.into_iter().collect();
        Self { first: all.iter().map(|d| d.0.clone()).min().unwrap_or_default(), last: all.iter().map(|d| d.0.clone()).max().unwrap_or_default(),
            open: all.into_iter().filter(|d| d.1).map(|d| d.0).collect() }
    }
    pub fn save(&self, p: &Path) -> Result<()> {
        self.save_as(p, "SSE")
    }
    /// Header keeps the range at whitespace token 4 (`load` parses it).
    pub fn save_as(&self, p: &Path, exchange: &str) -> Result<()> {
        let mut s = format!("# {exchange} trade calendar {}..{} (open days only)\n", self.first, self.last);
        for d in &self.open {
            s.push_str(d);
            s.push('\n');
        }
        Ok(std::fs::write(p, s)?)
    }
    pub fn load(p: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(p).with_context(|| format!("no calendar at {}; run `cmsq data calendar`", p.display()))?;
        let head = s.lines().next().unwrap_or("");
        let range = head.split_whitespace().nth(4).unwrap_or("..");
        let (a, b) = range.split_once("..").unwrap_or(("", ""));
        Ok(Self { first: a.into(), last: b.into(), open: s.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).map(String::from).collect() })
    }
    pub fn is_open(&self, d: &str) -> bool {
        self.open.contains(d)
    }
    /// (open days inside the bar range with no bar, bar days that are not open days)
    pub fn check(&self, bars: &[Bar]) -> (Vec<String>, Vec<String>) {
        let (Some(a), Some(b)) = (bars.first(), bars.last()) else { return (vec![], vec![]) };
        let have: BTreeSet<&str> = bars.iter().map(|x| x.date.as_str()).collect();
        let missing = self.open.range(a.date.clone()..=b.date.clone()).filter(|d| !have.contains(d.as_str())).cloned().collect();
        let extra = bars.iter().filter(|x| x.date >= self.first && x.date <= self.last && !self.is_open(&x.date)).map(|x| x.date.clone()).collect();
        (missing, extra)
    }
}

/// What a strategy sees on each bar: history up to and including the current bar. Nothing later exists.
pub struct Ctx<'a> {
    pub i: usize,
    pub history: &'a [Bar],
}
impl Ctx<'_> {
    pub fn bar(&self) -> &Bar {
        &self.history[self.i]
    }
    pub fn closes(&self, n: usize) -> Option<&[Bar]> {
        (self.history.len() >= n).then(|| &self.history[self.history.len() - n..])
    }
}

pub trait Strategy {
    fn name(&self) -> String;
    /// Target position in [0,1] decided at the close of the current bar.
    fn on_bar(&mut self, ctx: &Ctx) -> f64;
}

pub struct BarPlayer<'a> {
    pub bars: &'a [Bar],
}

impl<'a> BarPlayer<'a> {
    pub fn new(bars: &'a [Bar]) -> Self {
        Self { bars }
    }
    /// Replays bar by bar; the slice handed to the strategy ends at the current bar,
    /// so look-ahead is impossible by construction.
    pub fn run(&self, s: &mut dyn Strategy) -> Vec<f64> {
        (0..self.bars.len()).map(|i| s.on_bar(&Ctx { i, history: &self.bars[..=i] }).clamp(0.0, 1.0)).collect()
    }
}

fn mean_close(b: &[Bar]) -> f64 {
    b.iter().map(|x| x.close).sum::<f64>() / b.len() as f64
}

pub struct MaCrossover {
    pub fast: usize,
    pub slow: usize,
}
impl Strategy for MaCrossover {
    fn name(&self) -> String {
        format!("ma_crossover(fast={}, slow={})", self.fast, self.slow)
    }
    fn on_bar(&mut self, c: &Ctx) -> f64 {
        match (c.closes(self.fast), c.closes(self.slow)) {
            (Some(f), Some(s)) if mean_close(f) > mean_close(s) => 1.0,
            _ => 0.0,
        }
    }
}

pub struct Momentum {
    pub lookback: usize,
}
impl Strategy for Momentum {
    fn name(&self) -> String {
        format!("momentum(lookback={})", self.lookback)
    }
    fn on_bar(&mut self, c: &Ctx) -> f64 {
        match c.closes(self.lookback + 1) {
            Some(w) if w.last().unwrap().close > w[0].close => 1.0,
            _ => 0.0,
        }
    }
}

pub struct BuyAndHold;
impl Strategy for BuyAndHold {
    fn name(&self) -> String {
        "buy_and_hold".into()
    }
    fn on_bar(&mut self, _c: &Ctx) -> f64 {
        1.0
    }
}

pub fn by_name(name: &str, params: &serde_json::Value) -> Result<Box<dyn Strategy>> {
    let u = |k: &str, d: usize| params.get(k).and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(d);
    Ok(match name {
        "ma_crossover" => Box::new(MaCrossover { fast: u("fast", 10), slow: u("slow", 30) }),
        "momentum" => Box::new(Momentum { lookback: u("lookback", 20) }),
        "buy_and_hold" => Box::new(BuyAndHold),
        _ => anyhow::bail!("unknown native strategy {name} (ma_crossover | momentum | buy_and_hold)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn b(d: &str, c: f64) -> Bar {
        Bar { date: d.into(), open: c, high: c, low: c, close: c, volume: 1.0, adj_factor: None }
    }
    struct Peek(Vec<usize>);
    impl Strategy for Peek {
        fn name(&self) -> String {
            "peek".into()
        }
        fn on_bar(&mut self, c: &Ctx) -> f64 {
            self.0.push(c.history.len());
            0.0
        }
    }
    #[test]
    fn player_never_exposes_future() {
        let bars: Vec<Bar> = (1..=9).map(|i| b(&format!("2024-01-0{i}"), i as f64)).collect();
        let mut p = Peek(vec![]);
        BarPlayer::new(&bars).run(&mut p);
        assert_eq!(p.0, (1..=9).collect::<Vec<_>>());
        let pos = BarPlayer::new(&bars).run(&mut MaCrossover { fast: 2, slow: 4 });
        assert_eq!(pos, vec![0., 0., 0., 1., 1., 1., 1., 1., 1.]);
    }
    #[test]
    fn calendar_check() {
        let cal = Calendar::from_days([("2024-01-02".into(), true), ("2024-01-03".into(), true), ("2024-01-04".into(), true), ("2024-01-06".into(), false)]);
        let (miss, extra) = cal.check(&[b("2024-01-02", 1.0), b("2024-01-04", 1.0), b("2024-01-06", 1.0)]);
        assert_eq!(miss, vec!["2024-01-03"]);
        assert_eq!(extra, vec!["2024-01-06"]);
        let d = tempfile::tempdir().unwrap();
        cal.save(&d.path().join("c")).unwrap();
        let c2 = Calendar::load(&d.path().join("c")).unwrap();
        assert_eq!(c2.open, cal.open);
        assert_eq!(c2.first, "2024-01-02");
    }
}
