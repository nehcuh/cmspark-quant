//! Point-in-time (look-ahead) probe, computed on the TRUSTED host.
//!
//! The host builds several data variants and runs each in its OWN fresh sandbox
//! container, in a random order. A container only ever sees one variant, so a
//! strategy cannot memoise across variants, cannot compare variants, and cannot
//! read the real series (it is never mounted). Variants:
//!   * `full`             the evaluation series
//!   * `trunc@k`          bars[..k]  (future removed)
//!   * `resample@k#i`     bars[..k] + a block-bootstrapped synthetic future with
//!     the SAME dates/length, returns drawn from the series' own past and clipped
//!     inside the daily price limit, so the variant is not distinguishable by date,
//!     length or by an impossible >10% move.
//!
//! For every variant, positions for bars < k must equal the `full` positions.
use crate::market::Bar;
use serde::{Deserialize, Serialize};

/// Tiny deterministic PRNG (xorshift64*), seeded from a v4 UUID per job.
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    pub fn from_uuid() -> Self {
        let u = uuid::Uuid::new_v4();
        let b = u.as_bytes();
        Self::new(u64::from_le_bytes(b[..8].try_into().unwrap()) ^ u64::from_le_bytes(b[8..].try_into().unwrap()))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    pub name: String,
    /// positions for bars [..cut] are compared against the full run
    pub cut: usize,
    #[serde(skip)]
    pub bars: Vec<Bar>,
}

/// Prices the container sees: hfq-adjusted (raw × factor) when a factor is
/// present — this uses only past information, unlike qfq which would leak the
/// latest factor. Volume unchanged. No adj column is passed in.
pub fn container_view(bars: &[Bar]) -> Vec<Bar> {
    bars.iter()
        .map(|b| {
            let f = b.adj_factor.unwrap_or(1.0);
            Bar { date: b.date.clone(), open: b.open * f, high: b.high * f, low: b.low * f, close: b.close * f, volume: b.volume, adj_factor: None }
        })
        .collect()
}

fn resample_future(view: &[Bar], k: usize, rng: &mut Rng, max_move: f64) -> Vec<Bar> {
    let mut out: Vec<Bar> = view[..k].to_vec();
    if k < 3 {
        return view.to_vec();
    }
    const BLOCK: usize = 5;
    let mut src = rng.below(k - 1) + 1;
    let mut left = 0;
    for t in k..view.len() {
        if left == 0 {
            src = rng.below(k - 1) + 1;
            left = BLOCK;
        }
        left -= 1;
        let s = src.min(k - 1);
        src = if src + 1 < k { src + 1 } else { 1 };
        let (b, pb) = (&view[s], &view[s - 1]);
        let pc = out[t - 1].close;
        let r = |x: f64| (x / pb.close).clamp(1.0 - max_move, 1.0 + max_move);
        let (o, c) = (pc * r(b.open), pc * r(b.close));
        let (h, l) = (pc * r(b.high), pc * r(b.low));
        out.push(Bar { date: view[t].date.clone(), open: o, high: h.max(o).max(c), low: l.min(o).min(c), close: c, volume: b.volume, adj_factor: None });
    }
    out
}

/// Per-job probe layout. Cut points are drawn from a seeded RNG for every job
/// (seed + cuts are written to the audit chain), so a strategy cannot know
/// in advance which bars are compared; tail cuts always cover the last bars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeConfig {
    /// random truncation cuts per job, uniform in [lo, hi] of the series
    pub random_cuts: usize,
    pub lo: f64,
    pub hi: f64,
    /// truncations at n - t for each t (always probe the tail)
    pub tail: Vec<usize>,
    /// block-bootstrap resamples per random cut
    pub resamples: usize,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self { random_cuts: 5, lo: 0.35, hi: 0.98, tail: vec![1, 2, 4], resamples: 1 }
    }
}

/// Choose cut indices inside [from, n). Returns (random cuts, tail cuts), deduplicated.
pub fn choose_cuts(n: usize, from: usize, cfg: &ProbeConfig, rng: &mut Rng) -> (Vec<usize>, Vec<usize>) {
    let lo = (((n as f64) * cfg.lo) as usize).max(from).max(10);
    let hi = (((n as f64) * cfg.hi) as usize).min(n.saturating_sub(1));
    let mut random = vec![];
    if hi > lo {
        for _ in 0..cfg.random_cuts * 4 {
            if random.len() == cfg.random_cuts {
                break;
            }
            let k = lo + rng.below(hi - lo + 1);
            if !random.contains(&k) {
                random.push(k);
            }
        }
    }
    random.sort();
    let mut tail: Vec<usize> = cfg.tail.iter().filter(|&&t| t < n && n - t >= from.max(10)).map(|t| n - t).filter(|k| !random.contains(k)).collect();
    tail.sort();
    tail.dedup();
    (random, tail)
}

/// Dates the container sees: a fixed weekday sequence starting 2000-01-03,
/// independent of the real calendar. Real dates (and therefore "how far is the
/// last bar from today") never enter the sandbox.
pub fn fake_dates(n: usize) -> Vec<String> {
    let mut d = chrono::NaiveDate::from_ymd_opt(2000, 1, 3).unwrap();
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        use chrono::Datelike;
        if d.weekday().number_from_monday() <= 5 {
            out.push(d.format("%Y-%m-%d").to_string());
        }
        d = d.succ_opt().unwrap();
    }
    out
}

fn anonymize(bars: &mut [Bar], fake: &[String]) {
    for (b, d) in bars.iter_mut().zip(fake) {
        b.date = d.clone();
    }
}

/// Build the shuffled variant plan. `view` is the container view of the
/// evaluation series. `trunc_cuts` get a truncation; `resample_cuts` also get
/// `resamples` bootstrap futures. Every variant carries fake dates.
pub fn plan(view: &[Bar], trunc_cuts: &[usize], resample_cuts: &[usize], resamples: usize, max_move: f64, rng: &mut Rng) -> Vec<Variant> {
    let n = view.len();
    let fake = fake_dates(n);
    let mut v = vec![Variant { name: "full".into(), cut: n, bars: view.to_vec() }];
    for &k in trunc_cuts {
        if k < 10 || k >= n {
            continue;
        }
        v.push(Variant { name: format!("trunc@{k}"), cut: k, bars: view[..k].to_vec() });
    }
    for &k in resample_cuts {
        if k < 10 || k >= n {
            continue;
        }
        for i in 0..resamples {
            v.push(Variant { name: format!("resample@{k}#{i}"), cut: k, bars: resample_future(view, k, rng, max_move) });
        }
    }
    for x in v.iter_mut() {
        anonymize(&mut x.bars, &fake);
    }
    rng.shuffle(&mut v);
    v
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProbeReport {
    pub detected: bool,
    pub variants: usize,
    pub containers: usize,
    pub order: Vec<String>,
    pub mismatches: Vec<serde_json::Value>,
}

pub fn compare(full: &[f64], variant: &Variant, pos: &[f64], dates: &[String]) -> Option<serde_json::Value> {
    let k = variant.cut.min(full.len()).min(pos.len());
    let diff: Vec<usize> = (0..k).filter(|&i| (full[i] - pos[i]).abs() > 1e-9).collect();
    if diff.is_empty() {
        None
    } else {
        Some(serde_json::json!({"variant": variant.name, "bars_differ": diff.len(), "first_bar": diff[0], "first": dates.get(diff[0]), "cut_bar": k, "cut_date": dates.get(k.saturating_sub(1))}))
    }
}

/// Parse and validate untrusted `positions.csv`: exact dates, finite, within [0,1].
pub fn parse_positions(text: &str, dates: &[String]) -> Result<Vec<f64>, String> {
    let mut lines = text.lines();
    if lines.next() != Some("date,position") {
        return Err("positions.csv: bad header".into());
    }
    let mut out = Vec::with_capacity(dates.len());
    for (i, line) in lines.enumerate() {
        let (d, p) = line.split_once(',').ok_or_else(|| format!("positions.csv line {}: malformed", i + 2))?;
        if i >= dates.len() || d != dates[i] {
            return Err(format!("positions.csv line {}: date {:?} does not match input bar", i + 2, d.chars().take(20).collect::<String>()));
        }
        let x: f64 = p.parse().map_err(|_| format!("positions.csv line {}: not a number", i + 2))?;
        if !x.is_finite() || !(0.0..=1.0).contains(&x) {
            return Err(format!("positions.csv line {}: position {x} outside [0,1]", i + 2));
        }
        out.push(x);
    }
    if out.len() != dates.len() {
        return Err(format!("positions.csv has {} rows, expected {}", out.len(), dates.len()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bars(n: usize) -> Vec<Bar> {
        (0..n).map(|i| {
            let c = 10.0 + (i as f64 * 0.3).sin();
            Bar { date: format!("2020-{:02}-{:02}", 1 + i / 28, 1 + i % 28), open: c, high: c * 1.01, low: c * 0.99, close: c, volume: 1e6, adj_factor: None }
        }).collect()
    }
    #[test]
    fn plan_is_shuffled_and_resamples_respect_limits() {
        let b = bars(200);
        let p1 = plan(&b, &[100, 140, 180], &[100, 140, 180], 2, 0.095, &mut Rng::new(1));
        let p2 = plan(&b, &[100, 140, 180], &[100, 140, 180], 2, 0.095, &mut Rng::new(2));
        assert_eq!(p1.len(), 10);
        let fake = fake_dates(200);
        assert!(p1.iter().all(|v| v.bars.iter().zip(&fake).all(|(x, d)| &x.date == d)), "containers only see fake dates");
        assert!(p1.iter().all(|v| v.bars[0].date == "2000-01-03"));
        assert_ne!(p1.iter().map(|v| &v.name).collect::<Vec<_>>(), p2.iter().map(|v| &v.name).collect::<Vec<_>>());
        for v in &p1 {
            if v.name.starts_with("resample") {
                assert_eq!(v.bars.len(), b.len());
                assert_eq!(v.bars.last().unwrap().date, fake[199]);
                assert!(v.bars[..v.cut].iter().zip(&b[..v.cut]).all(|(x, y)| x.close == y.close));
                for w in v.bars.windows(2) {
                    assert!((w[1].close / w[0].close - 1.0).abs() <= 0.0951);
                }
            }
        }
    }
    #[test]
    fn positions_validation() {
        let d = vec!["2020-01-01".to_string(), "2020-01-02".to_string()];
        assert_eq!(parse_positions("date,position\n2020-01-01,0\n2020-01-02,1\n", &d).unwrap(), vec![0.0, 1.0]);
        assert!(parse_positions("date,position\n2020-01-01,0\n", &d).is_err());
        assert!(parse_positions("date,position\n2020-01-01,0\n2020-01-03,1\n", &d).is_err());
        assert!(parse_positions("date,position\n2020-01-01,NaN\n2020-01-02,1\n", &d).is_err());
        assert!(parse_positions("date,position\n2020-01-01,2\n2020-01-02,1\n", &d).is_err());
        assert!(parse_positions("{\"status\":\"ok\"}", &d).is_err());
    }
    #[test]
    fn cuts_are_random_per_seed_and_cover_the_tail() {
        let cfg = ProbeConfig::default();
        let (r1, t1) = choose_cuts(1000, 200, &cfg, &mut Rng::new(7));
        let (r2, _) = choose_cuts(1000, 200, &cfg, &mut Rng::new(8));
        assert_eq!(r1.len(), 5);
        assert_ne!(r1, r2);
        assert!(r1.iter().all(|&k| (350..=980).contains(&k)));
        assert_eq!(t1, vec![996, 998, 999]);
        // vault: cuts restricted to the vault window
        let (r, _) = choose_cuts(1000, 800, &cfg, &mut Rng::new(9));
        assert!(r.iter().all(|&k| k >= 800));
    }
}
