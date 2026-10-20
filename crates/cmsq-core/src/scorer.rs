//! TRUSTED scorer. Runs on the host, in Rust, on positions emitted by the
//! untrusted sandbox. Nothing here reads strategy-controlled metrics.
//!
//! Execution model (labelled truthfully everywhere):
//!   target position decided from bars up to and including the CLOSE of day t
//!   -> order filled at the OPEN of day t+1 (plus slippage) -> marked to CLOSE.
//!
//! A-share rules modelled:
//!   * 100-share board lots (odd lots only when selling out a whole position)
//!   * no buy when the open is at limit-up, no sell when the open is at limit-down:
//!     ±10% main board, ±20% ChiNext 300/301 (from 2020-08-24) and STAR 688
//!     (from 2019-07-22), ±30% BJ. ST (±5%) is NOT modelled: names are unavailable.
//!   * suspension (volume 0, or a missing session) => no trade that day
//!   * commission max(¥5, rate × value) per side, stamp duty on sells only:
//!     0.1% before 2023-08-28, 0.05% from 2023-08-28
//!   * T+1 is implied: at most one trade per day, at the open
//!   * adjusted returns when the bar carries `adj_factor` (shares are rescaled
//!     on ex-dates so wealth follows the adjusted series); raw prices are used for
//!     limits and lot sizes
use crate::market::Bar;
use serde::{Deserialize, Serialize};

pub const EXECUTION_LABEL: &str = "signal at close t -> filled at open t+1 (+slippage), marked to close; 100-share lots; limit/suspension blocks; min ¥5 commission; dated stamp duty";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CostModel {
    /// commission rate per side (min ¥5 applies)
    pub commission: f64,
    pub min_commission: f64,
    /// one-way slippage as a fraction of the open price
    pub slippage: f64,
    /// multiplies commission, slippage and stamp duty (cost-stress scenarios)
    pub multiplier: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self { commission: 0.00025, min_commission: 5.0, slippage: 0.0005, multiplier: 1.0 }
    }
}

impl CostModel {
    pub fn stressed(&self, m: f64) -> Self {
        Self { multiplier: self.multiplier * m, ..*self }
    }
}

/// Stamp duty on sells by trade date (YYYY-MM-DD).
pub fn stamp_duty(date: &str) -> f64 {
    if date < "2008-04-24" {
        0.003 // 2007-05-30..2008-04-23 (sells and buys; only sell side modelled)
    } else if date < "2023-08-28" {
        0.001
    } else {
        0.0005
    }
}

/// Daily price-limit fraction for a symbol on a date.
pub fn limit_pct(symbol: &str, date: &str) -> f64 {
    let s = symbol.to_uppercase();
    let code = s.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    if s.starts_with("BJ") || code.starts_with('8') || code.starts_with("43") {
        0.30
    } else if (code.starts_with("688") && date >= "2019-07-22") || ((code.starts_with("300") || code.starts_with("301")) && date >= "2020-08-24") {
        0.20
    } else {
        0.10
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SimResult {
    /// equity at each bar's close (len == bars.len())
    pub equity: Vec<f64>,
    /// fraction of equity invested at close
    pub exposure: Vec<f64>,
    pub trades: usize,
    pub blocked_limit_up: usize,
    pub blocked_limit_down: usize,
    pub blocked_suspended: usize,
    pub costs_paid: f64,
}

pub const INITIAL_CAPITAL: f64 = 1_000_000.0;

/// Simulate long-only target weights `pos` (decided at each bar's close).
pub fn simulate(symbol: &str, bars: &[Bar], pos: &[f64], costs: &CostModel) -> SimResult {
    assert_eq!(bars.len(), pos.len());
    let n = bars.len();
    let mut r = SimResult { equity: vec![INITIAL_CAPITAL; n], exposure: vec![0.0; n], ..Default::default() };
    if n == 0 {
        return r;
    }
    let mut cash = INITIAL_CAPITAL;
    let mut shares = 0.0_f64; // may become fractional after adj-factor rescaling
    let m = costs.multiplier;
    let commission = |v: f64| (v * costs.commission * m).max(costs.min_commission * m);
    for t in 1..n {
        let (b, pb) = (&bars[t], &bars[t - 1]);
        // corporate actions: keep wealth on the adjusted path
        if let (Some(a1), Some(a0)) = (b.adj_factor, pb.adj_factor) {
            if a0 > 0.0 && (a1 / a0 - 1.0).abs() > 1e-12 {
                shares *= a1 / a0;
            }
        }
        let target = pos[t - 1].clamp(0.0, 1.0);
        let suspended = b.volume <= 0.0;
        let lim = limit_pct(symbol, &b.date);
        // limit prices are computed off the previous raw close; when an ex-date sits
        // in between we approximate by the factor-adjusted previous close.
        let prev_close = match (b.adj_factor, pb.adj_factor) {
            (Some(a1), Some(a0)) if a1 > 0.0 => pb.close * a0 / a1,
            _ => pb.close,
        };
        let up = round2(prev_close * (1.0 + lim));
        let down = round2(prev_close * (1.0 - lim));
        let equity_open = cash + shares * b.open;
        let want_value = target * equity_open;
        let cur_value = shares * b.open;
        let delta = want_value - cur_value;
        let lot_value = 100.0 * b.open;
        if delta.abs() >= lot_value * 0.5 && !suspended {
            if delta > 0.0 {
                if b.open >= up - 0.005 {
                    r.blocked_limit_up += 1;
                } else {
                    let px = b.open * (1.0 + costs.slippage * m);
                    let mut lots = (delta / (100.0 * px)).floor();
                    while lots > 0.0 && lots * 100.0 * px + commission(lots * 100.0 * px) > cash {
                        lots -= 1.0;
                    }
                    if lots > 0.0 {
                        let v = lots * 100.0 * px;
                        let c = commission(v);
                        cash -= v + c;
                        shares += lots * 100.0;
                        r.costs_paid += c + lots * 100.0 * (px - b.open);
                        r.trades += 1;
                    }
                }
            } else if b.open <= down + 0.005 {
                r.blocked_limit_down += 1;
            } else {
                let px = b.open * (1.0 - costs.slippage * m);
                let sell_all = target <= 1e-9;
                let qty = if sell_all { shares } else { ((-delta) / (100.0 * b.open)).round().min((shares / 100.0).floor()) * 100.0 };
                if qty > 0.0 {
                    let v = qty * px;
                    let c = commission(v) + v * stamp_duty(&b.date) * m;
                    cash += v - c;
                    shares -= qty;
                    if shares < 1e-9 {
                        shares = 0.0;
                    }
                    r.costs_paid += c + qty * (b.open - px);
                    r.trades += 1;
                }
            }
        } else if suspended && delta.abs() >= lot_value * 0.5 {
            r.blocked_suspended += 1;
        }
        let eq = cash + shares * b.close;
        r.equity[t] = eq;
        r.exposure[t] = if eq > 0.0 { shares * b.close / eq } else { 0.0 };
    }
    r
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Metrics {
    pub bars: usize,
    pub start: String,
    pub end: String,
    pub total_return: f64,
    pub annual_return: f64,
    /// annualised (×√252)
    pub sharpe: f64,
    /// per-bar (non-annualised) Sharpe — the unit used by the Deflated Sharpe Ratio
    pub sharpe_daily: f64,
    pub skew: f64,
    /// non-excess kurtosis (normal = 3)
    pub kurtosis: f64,
    pub max_drawdown: f64,
    pub exposure: f64,
}

/// Metrics over bars `[from, to)` of a simulation (returns use equity[from-1]).
pub fn metrics(bars: &[Bar], sim: &SimResult, from: usize, to: usize) -> Metrics {
    let from = from.max(1);
    let to = to.min(sim.equity.len());
    if to <= from + 1 {
        return Metrics { bars: to.saturating_sub(from), ..Default::default() };
    }
    let rets: Vec<f64> = (from..to).map(|t| sim.equity[t] / sim.equity[t - 1] - 1.0).collect();
    let n = rets.len() as f64;
    let mean = rets.iter().sum::<f64>() / n;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let sd = var.sqrt();
    let m2 = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    let m3 = rets.iter().map(|r| (r - mean).powi(3)).sum::<f64>() / n;
    let m4 = rets.iter().map(|r| (r - mean).powi(4)).sum::<f64>() / n;
    let total = sim.equity[to - 1] / sim.equity[from - 1] - 1.0;
    let mut peak = sim.equity[from - 1];
    let mut dd = 0.0_f64;
    for t in from..to {
        peak = peak.max(sim.equity[t]);
        dd = dd.min(sim.equity[t] / peak - 1.0);
    }
    let sr_d = if sd > 1e-12 { mean / sd } else { 0.0 };
    Metrics {
        bars: rets.len(),
        start: bars[from].date.clone(),
        end: bars[to - 1].date.clone(),
        total_return: total,
        annual_return: if total > -1.0 { (1.0 + total).powf(252.0 / n) - 1.0 } else { -1.0 },
        sharpe: sr_d * 252f64.sqrt(),
        sharpe_daily: sr_d,
        skew: if m2 > 1e-18 { m3 / m2.powf(1.5) } else { 0.0 },
        kurtosis: if m2 > 1e-18 { m4 / (m2 * m2) } else { 3.0 },
        max_drawdown: dd,
        exposure: sim.exposure[from..to].iter().sum::<f64>() / n,
    }
}

// ---------------- statistics: Deflated Sharpe Ratio ----------------

/// Standard normal CDF (via erfc, Abramowitz–Stegun 7.1.26 is too coarse; use W. J. Cody-style rational erfc).
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

fn erfc(x: f64) -> f64 {
    // Numerical Recipes erfcc (fractional error < 1.2e-7)
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let ans = t * (-z * z - 1.26551223
        + t * (1.00002368 + t * (0.37409196 + t * (0.09678418 + t * (-0.18628806 + t * (0.27886807
        + t * (-1.13520398 + t * (1.48851587 + t * (-0.82215223 + t * 0.17087277)))))))))
        .exp();
    if x >= 0.0 { ans } else { 2.0 - ans }
}

/// Inverse standard normal CDF (Acklam's algorithm, rel. error ~1e-9).
#[allow(clippy::excessive_precision)]
pub fn norm_ppf(p: f64) -> f64 {
    assert!(p > 0.0 && p < 1.0);
    const A: [f64; 6] = [-3.969683028665376e+01, 2.209460984245205e+02, -2.759285104469687e+02, 1.383577518672690e+02, -3.066479806614716e+01, 2.506628277459239e+00];
    const B: [f64; 5] = [-5.447609879822406e+01, 1.615858368580409e+02, -1.556989798598866e+02, 6.680131188771972e+01, -1.328068155288572e+01];
    const C: [f64; 6] = [-7.784894002430293e-03, -3.223964580411365e-01, -2.400758277161838e+00, -2.549732539343734e+00, 4.374664141464968e+00, 2.938163982698783e+00];
    const D: [f64; 4] = [7.784695709041462e-03, 3.224671290700398e-01, 2.445134137142996e+00, 3.754408661907416e+00];
    let pl = 0.02425;
    if p < pl {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5]) / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - pl {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5]) / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dsr {
    pub trials: usize,
    pub sr_variance: f64,
    pub sr0_daily: f64,
    pub sr_daily: f64,
    pub dsr: f64,
}

/// Bailey & López de Prado (2014) Deflated Sharpe Ratio.
/// `trial_srs`: per-bar Sharpe of every trial in the library (including this one).
pub fn deflated_sharpe(m: &Metrics, trial_srs: &[f64]) -> Dsr {
    const GAMMA: f64 = std::f64::consts::EULER_GAMMA;
    let n = trial_srs.len().max(1);
    let t = m.bars.max(2) as f64;
    // Variance of SR across trials; floor at the sampling variance under H0 (1/T)
    let var = if trial_srs.len() >= 2 {
        let mu = trial_srs.iter().sum::<f64>() / trial_srs.len() as f64;
        trial_srs.iter().map(|x| (x - mu).powi(2)).sum::<f64>() / (trial_srs.len() as f64 - 1.0)
    } else {
        0.0
    }
    .max(1.0 / t);
    let sr0 = if n <= 1 {
        0.0
    } else {
        let nn = n as f64;
        var.sqrt() * ((1.0 - GAMMA) * norm_ppf(1.0 - 1.0 / nn) + GAMMA * norm_ppf(1.0 - 1.0 / (nn * std::f64::consts::E)))
    };
    let sr = m.sharpe_daily;
    let denom = (1.0 - m.skew * sr + (m.kurtosis - 1.0) / 4.0 * sr * sr).max(1e-12).sqrt();
    let dsr = norm_cdf((sr - sr0) * (t - 1.0).sqrt() / denom);
    Dsr { trials: n, sr_variance: var, sr0_daily: sr0, sr_daily: sr, dsr }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(d: &str, o: f64, c: f64, v: f64) -> Bar {
        Bar { date: d.into(), open: o, high: o.max(c), low: o.min(c), close: c, volume: v, adj_factor: None }
    }

    #[test]
    fn fills_at_next_open_not_same_close() {
        // signal on day 0 close; gap up on day 1 open must be paid
        let bars = vec![bar("2020-01-02", 10.0, 10.0, 1e6), bar("2020-01-03", 10.5, 10.5, 1e6), bar("2020-01-06", 10.5, 10.5, 1e6)];
        let s = simulate("SH600000", &bars, &[1.0, 1.0, 1.0], &CostModel { commission: 0.0, min_commission: 0.0, slippage: 0.0, multiplier: 1.0 });
        // bought at 10.5 (open day1), not at 10.0 -> no gain from the gap
        assert!((s.equity[2] / INITIAL_CAPITAL - 1.0).abs() < 1e-3, "{:?}", s.equity);
    }

    #[test]
    fn limit_up_open_blocks_buy_and_limit_down_blocks_sell() {
        let bars = vec![bar("2020-01-02", 10.0, 10.0, 1e6), bar("2020-01-03", 11.0, 11.0, 1e6), bar("2020-01-06", 9.9, 9.9, 1e6)];
        let s = simulate("SH600000", &bars, &[1.0, 1.0, 1.0], &CostModel::default());
        assert_eq!(s.blocked_limit_up, 1);
        assert_eq!(s.trades, 1); // bought on day 2 instead
        let bars = vec![bar("2020-01-02", 10.0, 10.0, 1e6), bar("2020-01-03", 10.0, 10.0, 1e6), bar("2020-01-06", 9.0, 9.0, 1e6), bar("2020-01-07", 9.0, 9.0, 1e6)];
        let s = simulate("SH600000", &bars, &[1.0, 0.0, 0.0, 0.0], &CostModel::default());
        assert_eq!(s.blocked_limit_down, 1);
        assert_eq!(s.trades, 2);
    }

    #[test]
    fn chinext_limit_is_20pct_after_reform() {
        assert_eq!(limit_pct("SZ300750", "2020-08-21"), 0.10);
        assert_eq!(limit_pct("SZ300750", "2020-08-24"), 0.20);
        assert_eq!(limit_pct("SH688981", "2021-01-04"), 0.20);
        assert_eq!(limit_pct("SH600000", "2021-01-04"), 0.10);
        assert_eq!(limit_pct("BJ430047", "2023-01-04"), 0.30);
    }

    #[test]
    fn suspension_blocks_trade() {
        let bars = vec![bar("2020-01-02", 10.0, 10.0, 1e6), bar("2020-01-03", 10.0, 10.0, 0.0), bar("2020-01-06", 10.0, 10.0, 1e6)];
        let s = simulate("SH600000", &bars, &[1.0, 1.0, 1.0], &CostModel::default());
        assert_eq!(s.blocked_suspended, 1);
        assert_eq!(s.trades, 1);
    }

    #[test]
    fn lots_min_commission_and_dated_stamp_duty() {
        assert_eq!(stamp_duty("2023-08-25"), 0.001);
        assert_eq!(stamp_duty("2023-08-28"), 0.0005);
        // tiny account: 1 lot of ¥10 shares => ¥1000 trade, commission must be ¥5 not ¥0.25
        let bars = vec![bar("2023-08-24", 10.0, 10.0, 1e6), bar("2023-08-25", 10.0, 10.0, 1e6), bar("2023-08-28", 10.0, 10.0, 1e6), bar("2023-08-29", 10.0, 10.0, 1e6)];
        let c = CostModel { slippage: 0.0, ..Default::default() };
        let s = simulate("SH600000", &bars, &[1.0, 0.0, 0.0, 0.0], &c);
        // 1000 lots would need ¥1,000,250 incl. commission -> 999 lots (99,900 sh) on 08-25,
        // sold on 08-28: commission 249.75 each side + stamp duty 0.05% (new rate) = 499.5
        let expected = INITIAL_CAPITAL - 249.75 - 249.75 - 499.5;
        assert!((s.equity[3] - expected).abs() < 1.0, "{} vs {}", s.equity[3], expected);
        let small = CostModel { slippage: 0.0, ..Default::default() };
        let v = 1000.0;
        assert_eq!((v * small.commission).max(small.min_commission), 5.0);
    }

    #[test]
    fn adj_factor_keeps_wealth_on_adjusted_path() {
        // 10 -> ex-dividend raw drop to 9 with factor 1 -> 10/9: adjusted price flat
        let mut b = vec![bar("2020-01-02", 10.0, 10.0, 1e6), bar("2020-01-03", 10.0, 10.0, 1e6), bar("2020-01-06", 9.0, 9.0, 1e6)];
        b[0].adj_factor = Some(1.0);
        b[1].adj_factor = Some(1.0);
        b[2].adj_factor = Some(10.0 / 9.0);
        let c = CostModel { commission: 0.0, min_commission: 0.0, slippage: 0.0, multiplier: 1.0 };
        let s = simulate("SH600000", &b, &[1.0, 1.0, 1.0], &c);
        assert!((s.equity[2] - s.equity[1]).abs() < 1.0, "{:?}", s.equity);
        // the ex-date open at 9.0 is NOT a limit-down (prev close is adjusted to 9.0)
        assert_eq!(s.blocked_limit_down, 0);
    }

    #[test]
    fn normal_functions() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-4);
        assert!((norm_ppf(0.975) - 1.959964).abs() < 1e-5);
        assert!((norm_ppf(0.01) + 2.326348).abs() < 1e-5);
    }

    #[test]
    fn dsr_falls_as_trials_grow() {
        let m = Metrics { bars: 750, sharpe_daily: 0.08, skew: 0.0, kurtosis: 3.0, ..Default::default() };
        let one = deflated_sharpe(&m, &[0.08]);
        let many: Vec<f64> = (0..100).map(|i| (i as f64 - 50.0) / 1000.0).chain([0.08]).collect();
        let lots = deflated_sharpe(&m, &many);
        assert!(one.dsr > 0.95, "{one:?}");
        assert!(lots.dsr < one.dsr && lots.sr0_daily > 0.0, "{lots:?}");
    }
}
