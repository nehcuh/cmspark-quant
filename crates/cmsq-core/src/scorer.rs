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
        Self {
            commission: 0.00025,
            min_commission: 5.0,
            slippage: 0.0005,
            multiplier: 1.0,
        }
    }
}

impl CostModel {
    pub fn stressed(&self, m: f64) -> Self {
        Self {
            multiplier: self.multiplier * m,
            ..*self
        }
    }
}

/// Stamp duty on sells by trade date (YYYY-MM-DD).
/// A-share stamp duty `(buy, sell)` rate in force on `date`.
/// History: 0.5% both sides from 1997-05-12; 0.4% from 1998-06-12; 0.2% from
/// 2001-11-16; 0.1% from 2005-01-24; 0.3% from 2007-05-30; 0.1% from 2008-04-24;
/// sell side only (0.1%) from 2008-09-19; 0.05% sell side from 2023-08-28.
pub fn stamp_duty_sides(date: &str) -> (f64, f64) {
    let both = |r: f64| (r, r);
    if date < "1998-06-12" {
        both(0.005)
    } else if date < "2001-11-16" {
        both(0.004)
    } else if date < "2005-01-24" {
        both(0.002)
    } else if date < "2007-05-30" {
        both(0.001)
    } else if date < "2008-04-24" {
        both(0.003)
    } else if date < "2008-09-19" {
        both(0.001)
    } else if date < "2023-08-28" {
        (0.0, 0.001)
    } else {
        (0.0, 0.0005)
    }
}

/// Sell-side stamp duty on `date`.
pub fn stamp_duty(date: &str) -> f64 {
    stamp_duty_sides(date).1
}

/// Per-symbol trading-rule facts (from the audited data fetch).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Rules {
    /// inclusive date ranges during which the name carried ST / *ST
    #[serde(default)]
    pub st: Vec<(String, String)>,
    /// listing date YYYY-MM-DD
    #[serde(default)]
    pub list_date: Option<String>,
}

impl Rules {
    pub fn is_st(&self, date: &str) -> bool {
        self.st
            .iter()
            .any(|(a, b)| date >= a.as_str() && date <= b.as_str())
    }
}

fn board(symbol: &str) -> (&'static str, String) {
    let s = symbol.to_uppercase();
    let code = s
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .to_string();
    let b = if s.starts_with("BJ")
        || code.starts_with("92")
        || code.starts_with('8')
        || code.starts_with("43")
    {
        "bj"
    } else if code.starts_with("688") || code.starts_with("689") {
        "star"
    } else if code.starts_with("300") || code.starts_with("301") {
        "chinext"
    } else {
        "main"
    };
    (b, code)
}

/// Daily price-limit fraction for a symbol on a date (non-ST, not an IPO day).
pub fn limit_pct(symbol: &str, date: &str) -> f64 {
    match board(symbol).0 {
        "bj" => 0.30,
        "star" if date >= "2019-07-22" => 0.20,
        "chinext" if date >= "2020-08-24" => 0.20,
        _ => 0.10,
    }
}

/// Limit with ST and IPO rules; `None` = no price limit that day.
/// `listing_day` is the 0-based trading-day index since listing when known.
pub fn limit_for(
    symbol: &str,
    date: &str,
    rules: &Rules,
    listing_day: Option<usize>,
) -> Option<f64> {
    let (b, _) = board(symbol);
    if let (Some(k), Some(ld)) = (listing_day, rules.list_date.as_deref()) {
        let registration = match b {
            "star" => ld >= "2019-07-22",
            "chinext" => ld >= "2020-08-24",
            "main" => ld >= "2023-04-10",
            _ => false,
        };
        // registration-regime IPOs: no limit for the first 5 trading days;
        // BJ and older IPOs: first day treated as unlimited (old 44%/-36% band not modelled)
        if (registration && k < 5) || k == 0 {
            return None;
        }
    }
    let base = limit_pct(symbol, date);
    if rules.is_st(date) && b == "main" {
        // main-board ST band 5%, widened to 10% from 2025-07-07
        return Some(if date >= "2025-07-07" { 0.10 } else { 0.05 });
    }
    Some(base)
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SimResult {
    /// futures: rolls held through without an executable old-leg price (display-only result)
    pub roll_unpriced: usize,
    /// equity at each bar's close (len == bars.len())
    pub equity: Vec<f64>,
    /// fraction of equity invested at close
    pub exposure: Vec<f64>,
    pub trades: usize,
    pub blocked_limit_up: usize,
    pub blocked_limit_down: usize,
    pub blocked_suspended: usize,
    pub costs_paid: f64,
    /// every executed fill, in order (feeds trade-level stats and QIFI export; never the gate)
    #[serde(default)]
    pub fills: Vec<Fill>,
    /// futures: lots closed under the close-today tier, and the fees that tier charged
    #[serde(default)]
    pub close_today_lots: f64,
    #[serde(default)]
    pub close_today_fees: f64,
    /// units held after the last bar (A-shares: shares; futures: signed lots) and the last close
    #[serde(default)]
    pub final_units: f64,
    #[serde(default)]
    pub final_price: f64,
}

/// One executed fill as seen by the trusted host simulator.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Fill {
    /// bar index
    pub t: usize,
    pub date: String,
    /// signed quantity (shares or lots; + buys)
    pub qty: f64,
    /// raw fill price incl. slippage
    pub price: f64,
    /// price on the adjusted (continuous) scale, for round-trip P/L across corporate actions / rolls
    pub adj_price: f64,
    /// fees + taxes + slippage cost of this fill
    pub cost: f64,
    /// buy / sell (A-shares) or open / close / close_today / roll (futures)
    pub kind: String,
}

/// Capacity estimate (CNY): for every fill in bars `[lo, hi)` (roll legs
/// excluded), the account size at which that fill would be `participation` of
/// the day's traded value; the estimate is the 10th percentile across fills
/// (90% of fills fit). `mult` = contract multiplier (1 for shares); A-share
/// fill quantities are on the adjusted scale, so notional uses `adj_price`.
pub fn capacity(
    bars: &[Bar],
    sim: &SimResult,
    mult: f64,
    lo: usize,
    hi: usize,
    participation: f64,
) -> Option<f64> {
    let mut caps: Vec<f64> = sim
        .fills
        .iter()
        .filter(|f| f.t >= lo.max(1) && f.t < hi && !f.kind.starts_with("roll"))
        .filter_map(|f| {
            let eq = *sim.equity.get(f.t - 1)?;
            let notional = if mult == 1.0 {
                (f.qty * f.adj_price).abs()
            } else {
                (f.qty * f.price * mult).abs()
            };
            let day_value = bars.get(f.t)?.volume * bars[f.t].close * mult;
            (notional > 0.0 && eq > 0.0 && day_value > 0.0)
                .then(|| participation * day_value / (notional / eq))
        })
        .collect();
    if caps.is_empty() {
        return None;
    }
    caps.sort_by(f64::total_cmp);
    Some(caps[((caps.len() - 1) as f64 * 0.1).floor() as usize])
}

pub const INITIAL_CAPITAL: f64 = 1_000_000.0;

/// Simulate long-only target weights `pos` (decided at each bar's close).
pub fn simulate(symbol: &str, bars: &[Bar], pos: &[f64], costs: &CostModel) -> SimResult {
    simulate_with(symbol, bars, pos, costs, &Rules::default())
}

/// `simulate` with ST / IPO rules. Prices in `bars` are RAW; lots, fees and limits
/// use raw prices, the adj_factor only carries wealth across corporate actions.
pub fn simulate_with(
    symbol: &str,
    bars: &[Bar],
    pos: &[f64],
    costs: &CostModel,
    rules: &Rules,
) -> SimResult {
    assert_eq!(bars.len(), pos.len());
    let first_listed = rules.list_date.as_deref().and_then(|ld| {
        let i = bars.iter().position(|b| b.date.as_str() >= ld)?;
        // only meaningful if the series actually starts at the listing
        (i == 0
            && chrono::NaiveDate::parse_from_str(&bars[0].date, "%Y-%m-%d")
                .ok()?
                .signed_duration_since(chrono::NaiveDate::parse_from_str(ld, "%Y-%m-%d").ok()?)
                .num_days()
                <= 7)
            .then_some(0usize)
    });
    let n = bars.len();
    let mut r = SimResult {
        equity: vec![INITIAL_CAPITAL; n],
        exposure: vec![0.0; n],
        ..Default::default()
    };
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
        let lim = limit_for(symbol, &b.date, rules, first_listed.map(|f| t - f));
        // limit prices are computed off the previous raw close; when an ex-date sits
        // in between we approximate by the factor-adjusted previous close.
        let prev_close = match (b.adj_factor, pb.adj_factor) {
            (Some(a1), Some(a0)) if a1 > 0.0 => pb.close * a0 / a1,
            _ => pb.close,
        };
        let (up, down) = match lim {
            Some(l) => (
                round2(prev_close * (1.0 + l)),
                round2(prev_close * (1.0 - l)),
            ),
            None => (f64::INFINITY, 0.0),
        };
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
                    let buy_duty = stamp_duty_sides(&b.date).0 * m;
                    let mut lots = (delta / (100.0 * px)).floor();
                    while lots > 0.0
                        && lots * 100.0 * px * (1.0 + buy_duty) + commission(lots * 100.0 * px)
                            > cash
                    {
                        lots -= 1.0;
                    }
                    if lots > 0.0 {
                        let v = lots * 100.0 * px;
                        let c = commission(v) + v * buy_duty;
                        cash -= v + c;
                        shares += lots * 100.0;
                        r.costs_paid += c + lots * 100.0 * (px - b.open);
                        let f = b.adj_factor.unwrap_or(1.0);
                        r.fills.push(Fill {
                            t,
                            date: b.date.clone(),
                            qty: lots * 100.0 / f,
                            price: px,
                            adj_price: px * f,
                            cost: c + lots * 100.0 * (px - b.open),
                            kind: "buy".into(),
                        });
                        r.trades += 1;
                    }
                }
            } else if b.open <= down + 0.005 {
                r.blocked_limit_down += 1;
            } else {
                let px = b.open * (1.0 - costs.slippage * m);
                let sell_all = target <= 1e-9;
                let qty = if sell_all {
                    shares
                } else {
                    ((-delta) / (100.0 * b.open))
                        .round()
                        .min((shares / 100.0).floor())
                        * 100.0
                };
                if qty > 0.0 {
                    let v = qty * px;
                    let c = commission(v) + v * stamp_duty(&b.date) * m;
                    let f = b.adj_factor.unwrap_or(1.0);
                    r.fills.push(Fill {
                        t,
                        date: b.date.clone(),
                        qty: -qty / f,
                        price: px,
                        adj_price: px * f,
                        cost: c + qty * (b.open - px),
                        kind: "sell".into(),
                    });
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
    r.final_units = shares;
    r.final_price = bars[n - 1].close;
    r
}

// ---------------- P1: multi-symbol portfolio (one account, shared cash) ----------------

/// Capital allocation across legs, decided at the close of day t from data up
/// to t only, applied at the next open (same timing as positions).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "scheme", rename_all = "snake_case")]
pub enum Alloc {
    Equal,
    /// weight ∝ 1 / trailing stdev of close-to-close returns over `lookback` bars
    InverseVol {
        lookback: usize,
    },
    /// fixed weights (normalised so they sum to ≤ 1)
    Fixed {
        weights: Vec<f64>,
    },
}

#[derive(Debug, Clone)]
pub struct Leg {
    pub symbol: String,
    /// aligned on common dates (same length for every leg)
    pub bars: Vec<Bar>,
    /// strategy target position in [0,1] for this leg
    pub pos: Vec<f64>,
    pub rules: Rules,
}

#[derive(Debug, Clone, Default)]
pub struct PortfolioSim {
    pub sim: SimResult,
    /// per leg: target weight of total equity decided at each close
    pub weights: Vec<Vec<f64>>,
}

fn adj_close(b: &Bar) -> f64 {
    b.close * b.adj_factor.unwrap_or(1.0)
}

/// Allocation weights per bar (sum ≤ 1), no look-ahead: row t uses bars ≤ t.
#[allow(clippy::needless_range_loop)]
pub fn allocation(legs: &[Leg], alloc: &Alloc) -> Vec<Vec<f64>> {
    let k = legs.len();
    let n = legs.first().map(|l| l.bars.len()).unwrap_or(0);
    let mut w = vec![vec![0.0; n]; k];
    for t in 0..n {
        let row: Vec<f64> = match alloc {
            Alloc::Equal => vec![1.0 / k as f64; k],
            Alloc::Fixed { weights } => {
                let s: f64 = weights.iter().map(|x| x.max(0.0)).sum();
                (0..k)
                    .map(|i| weights.get(i).copied().unwrap_or(0.0).max(0.0) / s.max(1.0))
                    .collect()
            }
            Alloc::InverseVol { lookback } => {
                let lb = (*lookback).max(5);
                let vols: Vec<Option<f64>> = legs
                    .iter()
                    .map(|l| {
                        let from = t.saturating_sub(lb);
                        let r: Vec<f64> = (from + 1..=t)
                            .filter(|&j| l.bars[j].volume > 0.0 && l.bars[j - 1].volume > 0.0)
                            .map(|j| adj_close(&l.bars[j]) / adj_close(&l.bars[j - 1]) - 1.0)
                            .collect();
                        if r.len() < lb / 2 {
                            return None;
                        }
                        let m = r.iter().sum::<f64>() / r.len() as f64;
                        let v = (r.iter().map(|x| (x - m).powi(2)).sum::<f64>()
                            / (r.len() as f64 - 1.0))
                            .sqrt();
                        (v > 1e-9).then_some(v)
                    })
                    .collect();
                if vols.iter().all(|v| v.is_some()) {
                    let inv: Vec<f64> = vols.iter().map(|v| 1.0 / v.unwrap()).collect();
                    let s: f64 = inv.iter().sum();
                    inv.iter().map(|x| x / s).collect()
                } else {
                    vec![1.0 / k as f64; k]
                }
            }
        };
        for i in 0..k {
            w[i][t] = row[i] * legs[i].pos[t].clamp(0.0, 1.0);
        }
    }
    w
}

/// One A-share account across several symbols: shared cash, sells before buys
/// each morning, the same lot / limit / suspension / fee / stamp-duty rules as
/// [`simulate_with`]. (The IPO-day "no limit" exemption is not applied here.)
pub fn simulate_portfolio(legs: &[Leg], alloc: &Alloc, costs: &CostModel) -> PortfolioSim {
    let k = legs.len();
    let n = legs.first().map(|l| l.bars.len()).unwrap_or(0);
    assert!(
        legs.iter().all(|l| l.bars.len() == n && l.pos.len() == n),
        "legs must be aligned"
    );
    let weights = allocation(legs, alloc);
    let mut r = SimResult {
        equity: vec![INITIAL_CAPITAL; n],
        exposure: vec![0.0; n],
        ..Default::default()
    };
    if n == 0 {
        return PortfolioSim { sim: r, weights };
    }
    let m = costs.multiplier;
    let commission = |v: f64| (v * costs.commission * m).max(costs.min_commission * m);
    let mut cash = INITIAL_CAPITAL;
    let mut shares = vec![0.0_f64; k];
    for t in 1..n {
        let mut bands = vec![(f64::INFINITY, 0.0); k];
        for i in 0..k {
            let (b, pb) = (&legs[i].bars[t], &legs[i].bars[t - 1]);
            if let (Some(a1), Some(a0)) = (b.adj_factor, pb.adj_factor) {
                if a0 > 0.0 && (a1 / a0 - 1.0).abs() > 1e-12 {
                    shares[i] *= a1 / a0;
                }
            }
            let prev_close = match (b.adj_factor, pb.adj_factor) {
                (Some(a1), Some(a0)) if a1 > 0.0 => pb.close * a0 / a1,
                _ => pb.close,
            };
            if let Some(l) = limit_for(&legs[i].symbol, &b.date, &legs[i].rules, None) {
                bands[i] = (
                    round2(prev_close * (1.0 + l)),
                    round2(prev_close * (1.0 - l)),
                );
            }
        }
        let equity_open: f64 = cash
            + (0..k)
                .map(|i| shares[i] * legs[i].bars[t].open)
                .sum::<f64>();
        let deltas: Vec<f64> = (0..k)
            .map(|i| weights[i][t - 1] * equity_open - shares[i] * legs[i].bars[t].open)
            .collect();
        // sells first (they fund the buys)
        for i in 0..k {
            let b = &legs[i].bars[t];
            let lot_value = 100.0 * b.open;
            if deltas[i] >= 0.0 || deltas[i].abs() < lot_value * 0.5 {
                continue;
            }
            if b.volume <= 0.0 {
                r.blocked_suspended += 1;
                continue;
            }
            if b.open <= bands[i].1 + 0.005 {
                r.blocked_limit_down += 1;
                continue;
            }
            let px = b.open * (1.0 - costs.slippage * m);
            let sell_all = weights[i][t - 1] <= 1e-9;
            let qty = if sell_all {
                shares[i]
            } else {
                ((-deltas[i]) / (100.0 * b.open))
                    .round()
                    .min((shares[i] / 100.0).floor())
                    * 100.0
            };
            if qty > 0.0 {
                let v = qty * px;
                let c = commission(v) + v * stamp_duty(&b.date) * m;
                cash += v - c;
                shares[i] -= qty;
                if shares[i] < 1e-9 {
                    shares[i] = 0.0;
                }
                r.costs_paid += c + qty * (b.open - px);
                r.trades += 1;
            }
        }
        for i in 0..k {
            let b = &legs[i].bars[t];
            let lot_value = 100.0 * b.open;
            if deltas[i] <= 0.0 || deltas[i] < lot_value * 0.5 {
                continue;
            }
            if b.volume <= 0.0 {
                r.blocked_suspended += 1;
                continue;
            }
            if b.open >= bands[i].0 - 0.005 {
                r.blocked_limit_up += 1;
                continue;
            }
            let px = b.open * (1.0 + costs.slippage * m);
            let buy_duty = stamp_duty_sides(&b.date).0 * m;
            let mut lots = (deltas[i] / (100.0 * px)).floor();
            while lots > 0.0
                && lots * 100.0 * px * (1.0 + buy_duty) + commission(lots * 100.0 * px) > cash
            {
                lots -= 1.0;
            }
            if lots > 0.0 {
                let v = lots * 100.0 * px;
                let c = commission(v) + v * buy_duty;
                cash -= v + c;
                shares[i] += lots * 100.0;
                r.costs_paid += c + lots * 100.0 * (px - b.open);
                r.trades += 1;
            }
        }
        let held: f64 = (0..k).map(|i| shares[i] * legs[i].bars[t].close).sum();
        let eq = cash + held;
        r.equity[t] = eq;
        r.exposure[t] = if eq > 0.0 { held / eq } else { 0.0 };
    }
    PortfolioSim { sim: r, weights }
}

/// Correlation matrix of the legs' gross strategy returns pos[t-1]·r[t].
pub fn leg_correlation(legs: &[Leg]) -> Vec<Vec<f64>> {
    let rets: Vec<Vec<f64>> = legs
        .iter()
        .map(|l| {
            (1..l.bars.len())
                .map(|t| {
                    l.pos[t - 1].clamp(0.0, 1.0)
                        * (adj_close(&l.bars[t]) / adj_close(&l.bars[t - 1]) - 1.0)
                })
                .collect()
        })
        .collect();
    let c = |a: &[f64], b: &[f64]| {
        let n = a.len().min(b.len()) as f64;
        let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
        let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
        for (x, y) in a.iter().zip(b) {
            ab += (x - ma) * (y - mb);
            aa += (x - ma).powi(2);
            bb += (y - mb).powi(2);
        }
        if aa < 1e-18 || bb < 1e-18 {
            0.0
        } else {
            ab / (aa * bb).sqrt()
        }
    };
    rets.iter()
        .map(|a| {
            rets.iter()
                .map(|b| (c(a, b) * 1000.0).round() / 1000.0)
                .collect()
        })
        .collect()
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
        return Metrics {
            bars: to.saturating_sub(from),
            ..Default::default()
        };
    }
    let rets: Vec<f64> = (from..to)
        .map(|t| sim.equity[t] / sim.equity[t - 1] - 1.0)
        .collect();
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
        annual_return: if total > -1.0 {
            (1.0 + total).powf(252.0 / n) - 1.0
        } else {
            -1.0
        },
        sharpe: sr_d * 252f64.sqrt(),
        sharpe_daily: sr_d,
        skew: if m2 > 1e-18 { m3 / m2.powf(1.5) } else { 0.0 },
        kurtosis: if m2 > 1e-18 { m4 / (m2 * m2) } else { 3.0 },
        max_drawdown: dd,
        // gross exposure: futures shorts count as exposure (A-share exposure is never negative)
        exposure: sim.exposure[from..to].iter().map(|x| x.abs()).sum::<f64>() / n,
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
    let ans = t
        * (-z * z - 1.26551223
            + t * (1.00002368
                + t * (0.37409196
                    + t * (0.09678418
                        + t * (-0.18628806
                            + t * (0.27886807
                                + t * (-1.13520398
                                    + t * (1.48851587 + t * (-0.82215223 + t * 0.17087277)))))))))
            .exp();
    if x >= 0.0 {
        ans
    } else {
        2.0 - ans
    }
}

/// Inverse standard normal CDF (Acklam's algorithm, rel. error ~1e-9).
#[allow(clippy::excessive_precision)]
pub fn norm_ppf(p: f64) -> f64 {
    assert!(p > 0.0 && p < 1.0);
    const A: [f64; 6] = [
        -3.969683028665376e+01,
        2.209460984245205e+02,
        -2.759285104469687e+02,
        1.383577518672690e+02,
        -3.066479806614716e+01,
        2.506628277459239e+00,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e+01,
        1.615858368580409e+02,
        -1.556989798598866e+02,
        6.680131188771972e+01,
        -1.328068155288572e+01,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e+00,
        -2.549732539343734e+00,
        4.374664141464968e+00,
        2.938163982698783e+00,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e+00,
        3.754408661907416e+00,
    ];
    let pl = 0.02425;
    if p < pl {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - pl {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
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
    deflated_sharpe_n(m, trial_srs, trial_srs.len())
}

/// DSR with `n_trials` independent tries but the cross-trial SR variance taken
/// from `trial_srs` only (callers drop runs that cheated via look-ahead: their
/// impossible Sharpe ratios are not draws from the honest search distribution).
pub fn deflated_sharpe_n(m: &Metrics, trial_srs: &[f64], n_trials: usize) -> Dsr {
    let var = if trial_srs.len() >= 2 {
        let mu = trial_srs.iter().sum::<f64>() / trial_srs.len() as f64;
        trial_srs.iter().map(|x| (x - mu).powi(2)).sum::<f64>() / (trial_srs.len() as f64 - 1.0)
    } else {
        0.0
    };
    deflated_sharpe_v(m, var, n_trials.max(trial_srs.len()))
}

/// Daily Sharpe above which a trial is treated as implausible (≈ annual 3.2) and
/// kept out of the variance pool.
pub const IMPLAUSIBLE_SR_DAILY: f64 = 0.20;

/// Robust cross-trial SR variance: drops implausible SRs, then uses
/// (1.4826·MAD)² once there are ≥5 trials (outliers cannot inflate it), else the
/// sample variance of the remaining pool. Returns (variance, kept, excluded).
pub fn robust_sr_variance(srs: &[f64]) -> (f64, usize, usize) {
    let mut v: Vec<f64> = srs
        .iter()
        .copied()
        .filter(|x| x.is_finite() && x.abs() <= IMPLAUSIBLE_SR_DAILY)
        .collect();
    let excluded = srs.len() - v.len();
    if v.len() < 2 {
        return (0.0, v.len(), excluded);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = |x: &[f64]| {
        if x.len() % 2 == 1 {
            x[x.len() / 2]
        } else {
            (x[x.len() / 2 - 1] + x[x.len() / 2]) / 2.0
        }
    };
    if v.len() >= 5 {
        let m = med(&v);
        let mut dev: Vec<f64> = v.iter().map(|x| (x - m).abs()).collect();
        dev.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sd = 1.4826 * med(&dev);
        return (sd * sd, v.len(), excluded);
    }
    let mu = v.iter().sum::<f64>() / v.len() as f64;
    (
        v.iter().map(|x| (x - mu).powi(2)).sum::<f64>() / (v.len() as f64 - 1.0),
        v.len(),
        excluded,
    )
}

/// DSR given the cross-trial SR variance and the effective number of trials.
pub fn deflated_sharpe_v(m: &Metrics, sr_var: f64, n_trials: usize) -> Dsr {
    const GAMMA: f64 = std::f64::consts::EULER_GAMMA;
    let n = n_trials.max(1);
    let t = m.bars.max(2) as f64;
    // floor at the sampling variance under H0 (1/T)
    let var = sr_var.max(1.0 / t);
    let sr0 = if n <= 1 {
        0.0
    } else {
        let nn = n as f64;
        var.sqrt()
            * ((1.0 - GAMMA) * norm_ppf(1.0 - 1.0 / nn)
                + GAMMA * norm_ppf(1.0 - 1.0 / (nn * std::f64::consts::E)))
    };
    let sr = m.sharpe_daily;
    let denom = (1.0 - m.skew * sr + (m.kurtosis - 1.0) / 4.0 * sr * sr)
        .max(1e-12)
        .sqrt();
    let dsr = norm_cdf((sr - sr0) * (t - 1.0).sqrt() / denom);
    Dsr {
        trials: n,
        sr_variance: var,
        sr0_daily: sr0,
        sr_daily: sr,
        dsr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(d: &str, o: f64, c: f64, v: f64) -> Bar {
        Bar {
            date: d.into(),
            open: o,
            high: o.max(c),
            low: o.min(c),
            close: c,
            volume: v,
            adj_factor: None,
        }
    }

    #[test]
    fn fills_at_next_open_not_same_close() {
        // signal on day 0 close; gap up on day 1 open must be paid
        let bars = vec![
            bar("2020-01-02", 10.0, 10.0, 1e6),
            bar("2020-01-03", 10.5, 10.5, 1e6),
            bar("2020-01-06", 10.5, 10.5, 1e6),
        ];
        let s = simulate(
            "SH600000",
            &bars,
            &[1.0, 1.0, 1.0],
            &CostModel {
                commission: 0.0,
                min_commission: 0.0,
                slippage: 0.0,
                multiplier: 1.0,
            },
        );
        // bought at 10.5 (open day1), not at 10.0 -> no gain from the gap
        assert!(
            (s.equity[2] / INITIAL_CAPITAL - 1.0).abs() < 1e-3,
            "{:?}",
            s.equity
        );
    }

    #[test]
    fn limit_up_open_blocks_buy_and_limit_down_blocks_sell() {
        let bars = vec![
            bar("2020-01-02", 10.0, 10.0, 1e6),
            bar("2020-01-03", 11.0, 11.0, 1e6),
            bar("2020-01-06", 9.9, 9.9, 1e6),
        ];
        let s = simulate("SH600000", &bars, &[1.0, 1.0, 1.0], &CostModel::default());
        assert_eq!(s.blocked_limit_up, 1);
        assert_eq!(s.trades, 1); // bought on day 2 instead
        let bars = vec![
            bar("2020-01-02", 10.0, 10.0, 1e6),
            bar("2020-01-03", 10.0, 10.0, 1e6),
            bar("2020-01-06", 9.0, 9.0, 1e6),
            bar("2020-01-07", 9.0, 9.0, 1e6),
        ];
        let s = simulate(
            "SH600000",
            &bars,
            &[1.0, 0.0, 0.0, 0.0],
            &CostModel::default(),
        );
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
        assert_eq!(limit_pct("BJ920002", "2024-11-04"), 0.30);
        assert_eq!(limit_pct("SH900901", "2024-11-04"), 0.10);
        let st = Rules {
            st: vec![("2020-01-01".into(), "2025-12-31".into())],
            list_date: None,
        };
        assert_eq!(limit_for("SZ000001", "2021-01-04", &st, None), Some(0.05));
        assert_eq!(limit_for("SZ000001", "2025-07-07", &st, None), Some(0.10));
        assert_eq!(limit_for("SZ300001", "2021-01-04", &st, None), Some(0.20));
        let ipo = Rules {
            st: vec![],
            list_date: Some("2023-06-01".into()),
        };
        assert_eq!(limit_for("SH603001", "2023-06-05", &ipo, Some(4)), None);
        assert_eq!(
            limit_for("SH603001", "2023-06-08", &ipo, Some(5)),
            Some(0.10)
        );
        let old = Rules {
            st: vec![],
            list_date: Some("2015-06-01".into()),
        };
        assert_eq!(limit_for("SH603002", "2015-06-01", &old, Some(0)), None);
        assert_eq!(
            limit_for("SH603002", "2015-06-02", &old, Some(1)),
            Some(0.10)
        );
    }

    #[test]
    fn suspension_blocks_trade() {
        let bars = vec![
            bar("2020-01-02", 10.0, 10.0, 1e6),
            bar("2020-01-03", 10.0, 10.0, 0.0),
            bar("2020-01-06", 10.0, 10.0, 1e6),
        ];
        let s = simulate("SH600000", &bars, &[1.0, 1.0, 1.0], &CostModel::default());
        assert_eq!(s.blocked_suspended, 1);
        assert_eq!(s.trades, 1);
    }

    #[test]
    fn lots_min_commission_and_dated_stamp_duty() {
        assert_eq!(stamp_duty("2023-08-25"), 0.001);
        assert_eq!(stamp_duty_sides("2006-01-04"), (0.001, 0.001));
        assert_eq!(stamp_duty_sides("2007-06-01"), (0.003, 0.003));
        assert_eq!(stamp_duty_sides("2008-05-05"), (0.001, 0.001));
        assert_eq!(stamp_duty_sides("2008-09-19"), (0.0, 0.001));
        assert_eq!(stamp_duty_sides("2003-01-02"), (0.002, 0.002));
        assert_eq!(stamp_duty("2023-08-28"), 0.0005);
        // tiny account: 1 lot of ¥10 shares => ¥1000 trade, commission must be ¥5 not ¥0.25
        let bars = vec![
            bar("2023-08-24", 10.0, 10.0, 1e6),
            bar("2023-08-25", 10.0, 10.0, 1e6),
            bar("2023-08-28", 10.0, 10.0, 1e6),
            bar("2023-08-29", 10.0, 10.0, 1e6),
        ];
        let c = CostModel {
            slippage: 0.0,
            ..Default::default()
        };
        let s = simulate("SH600000", &bars, &[1.0, 0.0, 0.0, 0.0], &c);
        // 1000 lots would need ¥1,000,250 incl. commission -> 999 lots (99,900 sh) on 08-25,
        // sold on 08-28: commission 249.75 each side + stamp duty 0.05% (new rate) = 499.5
        let expected = INITIAL_CAPITAL - 249.75 - 249.75 - 499.5;
        assert!(
            (s.equity[3] - expected).abs() < 1.0,
            "{} vs {}",
            s.equity[3],
            expected
        );
        let small = CostModel {
            slippage: 0.0,
            ..Default::default()
        };
        let v = 1000.0;
        assert_eq!((v * small.commission).max(small.min_commission), 5.0);
    }

    #[test]
    fn adj_factor_keeps_wealth_on_adjusted_path() {
        // 10 -> ex-dividend raw drop to 9 with factor 1 -> 10/9: adjusted price flat
        let mut b = vec![
            bar("2020-01-02", 10.0, 10.0, 1e6),
            bar("2020-01-03", 10.0, 10.0, 1e6),
            bar("2020-01-06", 9.0, 9.0, 1e6),
        ];
        b[0].adj_factor = Some(1.0);
        b[1].adj_factor = Some(1.0);
        b[2].adj_factor = Some(10.0 / 9.0);
        let c = CostModel {
            commission: 0.0,
            min_commission: 0.0,
            slippage: 0.0,
            multiplier: 1.0,
        };
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
        let m = Metrics {
            bars: 750,
            sharpe_daily: 0.08,
            skew: 0.0,
            kurtosis: 3.0,
            ..Default::default()
        };
        let one = deflated_sharpe(&m, &[0.08]);
        let many: Vec<f64> = (0..100)
            .map(|i| (i as f64 - 50.0) / 1000.0)
            .chain([0.08])
            .collect();
        let lots = deflated_sharpe(&m, &many);
        assert!(one.dsr > 0.95, "{one:?}");
        assert!(lots.dsr < one.dsr && lots.sr0_daily > 0.0, "{lots:?}");
    }
    #[test]
    fn dsr_cheater_excluded_from_variance_but_counted() {
        let m = Metrics {
            bars: 1500,
            sharpe_daily: 0.08,
            kurtosis: 3.0,
            ..Default::default()
        };
        let honest = [0.01, 0.03, 0.02, 0.04];
        let with_cheat = [0.01, 0.03, 0.02, 0.04, 0.41];
        let a = deflated_sharpe(&m, &with_cheat);
        let b = deflated_sharpe_n(&m, &honest, 5);
        assert_eq!(b.trials, 5);
        assert!(b.sr0_daily < a.sr0_daily && b.dsr > a.dsr, "{a:?} {b:?}");
    }
    #[test]
    fn robust_pool_ignores_planted_outliers() {
        let honest = [0.01, 0.02, 0.015, 0.03, 0.005, 0.025, 0.02];
        let mut planted = honest.to_vec();
        planted.extend([0.15, 0.16, 0.17, 0.5]);
        let (v0, _, _) = robust_sr_variance(&honest);
        let (v1, kept, ex) = robust_sr_variance(&planted);
        assert_eq!(ex, 1);
        assert_eq!(kept, 10);
        assert!(v1 < 4.0 * v0, "{v0} {v1}");
        let m = Metrics {
            bars: 800,
            sharpe_daily: 0.08,
            kurtosis: 3.0,
            ..Default::default()
        };
        // trial floor: N=1 is still deflated as if 20 trials were run
        assert!(deflated_sharpe_v(&m, 0.0, 20).sr0_daily > deflated_sharpe_v(&m, 0.0, 1).sr0_daily);
    }

    fn legs2() -> Vec<Leg> {
        let mk = |sym: &str, f: fn(usize) -> f64| {
            let bars: Vec<Bar> = (0..120)
                .map(|i| {
                    let c = f(i);
                    bar(
                        &format!("2021-{:02}-{:02}", 1 + i / 28, 1 + i % 28),
                        c,
                        c,
                        1e6,
                    )
                })
                .collect();
            Leg {
                symbol: sym.into(),
                pos: vec![1.0; bars.len()],
                bars,
                rules: Rules::default(),
            }
        };
        vec![
            mk("SH600000", |i| 10.0 + 0.02 * i as f64),
            mk("SZ000001", |i| 20.0 + if i % 2 == 0 { 0.6 } else { -0.6 }),
        ]
    }

    #[test]
    fn portfolio_shares_cash_and_never_overspends() {
        let legs = legs2();
        let p = simulate_portfolio(&legs, &Alloc::Equal, &CostModel::default());
        assert!(p.sim.trades >= 2);
        assert!(
            p.sim.exposure.iter().all(|x| *x <= 1.0 + 1e-9),
            "never more than 100% invested"
        );
        assert!(p.sim.exposure[5] > 0.95, "{:?}", &p.sim.exposure[..6]);
        // fixed weights summing to 0.5 keep half in cash
        let half = simulate_portfolio(
            &legs,
            &Alloc::Fixed {
                weights: vec![0.25, 0.25],
            },
            &CostModel::default(),
        );
        assert!(
            (half.sim.exposure[10] - 0.5).abs() < 0.05,
            "{}",
            half.sim.exposure[10]
        );
    }

    #[test]
    fn inverse_vol_tilts_to_the_calm_leg_without_lookahead() {
        let legs = legs2();
        let w = allocation(&legs, &Alloc::InverseVol { lookback: 20 });
        assert!(
            (w[0][3] - 0.5).abs() < 1e-9,
            "not enough history yet -> equal"
        );
        assert!(w[0][60] > w[1][60], "{} {}", w[0][60], w[1][60]);
        // changing a FUTURE bar must not change today's weight
        let mut l2 = legs.clone();
        l2[1].bars[61].close *= 3.0;
        assert_eq!(
            allocation(&l2, &Alloc::InverseVol { lookback: 20 })[0][60],
            w[0][60]
        );
    }

    #[test]
    fn single_leg_portfolio_matches_single_symbol_sim() {
        let mut l = legs2();
        l.truncate(1);
        let a = simulate_portfolio(&l, &Alloc::Equal, &CostModel::default());
        let b = simulate("SH600000", &l[0].bars, &l[0].pos, &CostModel::default());
        assert!((a.sim.equity.last().unwrap() - b.equity.last().unwrap()).abs() < 1.0);
    }

    #[test]
    fn capacity_is_p10_of_fill_sizes_vs_daily_value() {
        let bars: Vec<Bar> = (0..5)
            .map(|i| Bar {
                date: format!("2024-01-0{}", i + 1),
                open: 10.0,
                high: 10.0,
                low: 10.0,
                close: 10.0,
                volume: 1e6,
                adj_factor: Some(1.0),
            })
            .collect();
        let mut sim = SimResult {
            equity: vec![1e6; 5],
            ..Default::default()
        };
        // full-equity fill on a ¥10m-volume day: 5% * 1e7 / 1.0 = 5e5
        sim.fills.push(Fill {
            t: 1,
            date: "d".into(),
            qty: 1e5,
            price: 10.0,
            adj_price: 10.0,
            cost: 0.0,
            kind: "buy".into(),
        });
        assert_eq!(capacity(&bars, &sim, 1.0, 0, 5, 0.05), Some(5e5));
        // a 10%-of-equity fill is 10x roomier; p10 of two fills is the smaller
        sim.fills.push(Fill {
            t: 2,
            qty: 1e4,
            ..sim.fills[0].clone()
        });
        assert_eq!(capacity(&bars, &sim, 1.0, 0, 5, 0.05), Some(5e5));
        assert_eq!(capacity(&bars, &sim, 1.0, 2, 5, 0.05), Some(5e6), "window");
        assert_eq!(
            capacity(&bars, &SimResult::default(), 1.0, 0, 5, 0.05),
            None
        );
    }
}
