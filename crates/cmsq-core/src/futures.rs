//! P2: China futures rules for the TRUSTED host scorer.
//!
//! Symbols: `FUT_<PRODUCT>` (e.g. `FUT_RB`, `FUT_IF`) = main-contract continuous
//! series built by [`build_main_series`]; each bar carries the RAW prices of the
//! contract held that day, and `adj_factor` jumps on roll days (ratio
//! continuity, adjusted = raw × factor).
//!
//! Execution model (labelled everywhere): target position in [-1, 1] decided at
//! the close of trading day t → filled at the OPEN of trading day t+1 (for
//! products with a night session that open IS the night-session open, 21:00 of
//! the previous calendar day, because exchanges assign the night session to the
//! next trading day) → marked to close. Position 1 = notional exposure of
//! `leverage × equity` (default 1×, i.e. fully cash-collateralised).
//!
//! Rules modelled: contract multiplier, minimum tick (fills rounded to tick),
//! margin requirement (orders that would exceed equity are cut), limit
//! up/down from the previous trading day's close (no buying at limit-up open,
//! no selling at limit-down open), per-contract fees (per-lot or ratio, open /
//! close / close-today), main-contract roll (old contract closed and new one
//! opened at the roll-day open: both legs pay fees and slippage).
//! Close-today (平今): every open lot is tagged with its trading day; a close
//! consumes lots in the exchange's [`CloseOrder`] and lots opened on the same
//! trading day pay the close-today tier. At daily granularity a position is
//! always opened and closed on different trading days, so the tier only bites
//! in [`simulate_session`] with intraday bars (and after a roll, whose new
//! lots count as opened that day).
//! Simplifications (documented): limits use the previous close, not the
//! previous settlement; the overnight move on a roll day is taken from the new
//! contract (ratio continuity).
use crate::market::Bar;
use crate::scorer::{CostModel, Fill, SimResult, INITIAL_CAPITAL};
use serde::{Deserialize, Serialize};

pub const EXECUTION_LABEL: &str = "futures: signal at close t -> filled at open t+1 (night-session open where traded, +slippage), marked to close; integer lots × multiplier; tick-rounded fills; margin cap; limit up/down blocks; per-contract fees; main-contract roll pays both legs";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fee {
    /// yuan per lot
    PerLot { open: f64, close: f64, close_today: f64 },
    /// fraction of notional
    Ratio { open: f64, close: f64, close_today: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContractSpec {
    pub product: String,
    pub exchange: String,
    /// tushare exchange suffix (SHF, DCE, ZCE, CFX, INE, GFE)
    pub ts_suffix: String,
    pub multiplier: f64,
    pub tick: f64,
    pub margin_rate: f64,
    pub limit_pct: f64,
    pub fee: Fee,
    /// night session (start, end) local time; None = day session only
    pub night: Option<(String, String)>,
    /// which lots a close order consumes first; None = exchange default ([`default_close_order`])
    #[serde(default)]
    pub close_order: Option<CloseOrder>,
}

/// Order in which a closing trade consumes today's vs earlier lots (decides
/// whether the close-today fee tier applies).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloseOrder {
    /// oldest lots first (DCE / CZCE / GFEX default: 先开先平)
    Fifo,
    /// today's lots first (CFFEX default)
    TodayFirst,
    /// trader chooses CloseToday / CloseYesterday explicitly (SHFE / INE):
    /// modelled as the cheaper bucket first
    Cheapest,
}

/// Default close order per exchange. These are modelling assumptions, not
/// broker facts: override per product with `close_order` in `CMSQ_FUTURES_SPECS`.
pub fn default_close_order(exchange: &str) -> CloseOrder {
    match exchange {
        "SHFE" | "INE" => CloseOrder::Cheapest,
        "CFFEX" => CloseOrder::TodayFirst,
        _ => CloseOrder::Fifo,
    }
}

impl ContractSpec {
    pub fn close_order(&self) -> CloseOrder {
        self.close_order.unwrap_or_else(|| default_close_order(&self.exchange))
    }
}

/// Open lots of ONE side, tagged with the trading day they were opened.
#[derive(Debug, Clone, Default)]
struct Ledger {
    lots: std::collections::VecDeque<(String, f64)>,
}

impl Ledger {
    fn open(&mut self, day: &str, q: f64) {
        match self.lots.back_mut() {
            Some((d, x)) if d == day => *x += q,
            _ => self.lots.push_back((day.to_string(), q)),
        }
    }
    fn total(&self) -> f64 {
        self.lots.iter().map(|x| x.1).sum()
    }
    /// Close `q` lots on `day`; returns (earlier-day lots closed, today's lots closed).
    fn close(&mut self, day: &str, q: f64, order: CloseOrder, today_cheaper: bool) -> (f64, f64) {
        let today_first = match order {
            CloseOrder::Fifo => false,
            CloseOrder::TodayFirst => true,
            CloseOrder::Cheapest => today_cheaper,
        };
        let (mut left, mut his, mut tod) = (q, 0.0, 0.0);
        while left > 1e-9 {
            let idx = if today_first {
self.lots.iter().position(|x| x.0 == day).unwrap_or_default()
            } else {
self.lots.iter().position(|x| x.0 != day).unwrap_or_default()
            };
            let Some(slot) = self.lots.get_mut(idx) else { break };
            let take = slot.1.min(left);
            slot.1 -= take;
            left -= take;
            if slot.0 == day { tod += take } else { his += take }
            if slot.1 <= 1e-9 {
                self.lots.remove(idx);
            }
        }
        (his, tod)
    }
}

/// Defaults from exchange rules (2026); exchanges adjust margins/limits and
/// brokers add fees: override with `CMSQ_FUTURES_SPECS=<json file>`.
type SpecRow = (&'static str, &'static str, &'static str, f64, f64, f64, f64, Fee, Option<(&'static str, &'static str)>);
const TABLE: &[SpecRow] = &[
    ("RB", "SHFE", "SHF", 10.0, 1.0, 0.07, 0.05, Fee::Ratio { open: 1e-4, close: 1e-4, close_today: 1e-4 }, Some(("21:00", "23:00"))),
    ("HC", "SHFE", "SHF", 10.0, 1.0, 0.07, 0.05, Fee::Ratio { open: 1e-4, close: 1e-4, close_today: 1e-4 }, Some(("21:00", "23:00"))),
    ("CU", "SHFE", "SHF", 5.0, 10.0, 0.08, 0.06, Fee::Ratio { open: 5e-5, close: 5e-5, close_today: 1e-4 }, Some(("21:00", "01:00"))),
    ("AL", "SHFE", "SHF", 5.0, 5.0, 0.08, 0.06, Fee::PerLot { open: 3.0, close: 3.0, close_today: 0.0 }, Some(("21:00", "01:00"))),
    ("AU", "SHFE", "SHF", 1000.0, 0.02, 0.08, 0.06, Fee::PerLot { open: 10.0, close: 10.0, close_today: 0.0 }, Some(("21:00", "02:30"))),
    ("AG", "SHFE", "SHF", 15.0, 1.0, 0.09, 0.07, Fee::Ratio { open: 5e-5, close: 5e-5, close_today: 5e-5 }, Some(("21:00", "02:30"))),
    ("RU", "SHFE", "SHF", 10.0, 5.0, 0.08, 0.06, Fee::PerLot { open: 3.0, close: 3.0, close_today: 3.0 }, Some(("21:00", "23:00"))),
    ("SC", "INE", "INE", 1000.0, 0.1, 0.10, 0.08, Fee::PerLot { open: 20.0, close: 20.0, close_today: 0.0 }, Some(("21:00", "02:30"))),
    ("I", "DCE", "DCE", 100.0, 0.5, 0.11, 0.08, Fee::Ratio { open: 1e-4, close: 1e-4, close_today: 1e-4 }, Some(("21:00", "23:00"))),
    ("J", "DCE", "DCE", 100.0, 0.5, 0.20, 0.08, Fee::Ratio { open: 1e-4, close: 1e-4, close_today: 1.4e-4 }, Some(("21:00", "23:00"))),
    ("JM", "DCE", "DCE", 60.0, 0.5, 0.20, 0.08, Fee::Ratio { open: 1e-4, close: 1e-4, close_today: 1.4e-4 }, Some(("21:00", "23:00"))),
    ("M", "DCE", "DCE", 10.0, 1.0, 0.07, 0.05, Fee::PerLot { open: 1.5, close: 1.5, close_today: 1.5 }, Some(("21:00", "23:00"))),
    ("Y", "DCE", "DCE", 10.0, 2.0, 0.07, 0.05, Fee::PerLot { open: 2.5, close: 2.5, close_today: 2.5 }, Some(("21:00", "23:00"))),
    ("P", "DCE", "DCE", 10.0, 2.0, 0.08, 0.06, Fee::PerLot { open: 2.5, close: 2.5, close_today: 2.5 }, Some(("21:00", "23:00"))),
    ("SR", "CZCE", "ZCE", 10.0, 1.0, 0.07, 0.05, Fee::PerLot { open: 3.0, close: 3.0, close_today: 0.0 }, Some(("21:00", "23:00"))),
    ("CF", "CZCE", "ZCE", 5.0, 5.0, 0.07, 0.05, Fee::PerLot { open: 4.3, close: 4.3, close_today: 0.0 }, Some(("21:00", "23:00"))),
    ("TA", "CZCE", "ZCE", 5.0, 2.0, 0.07, 0.05, Fee::PerLot { open: 3.0, close: 3.0, close_today: 0.0 }, Some(("21:00", "23:00"))),
    ("MA", "CZCE", "ZCE", 10.0, 1.0, 0.07, 0.05, Fee::PerLot { open: 2.0, close: 2.0, close_today: 6.0 }, Some(("21:00", "23:00"))),
    ("IF", "CFFEX", "CFX", 300.0, 0.2, 0.12, 0.10, Fee::Ratio { open: 2.3e-5, close: 2.3e-5, close_today: 2.3e-4 }, None),
    ("IH", "CFFEX", "CFX", 300.0, 0.2, 0.12, 0.10, Fee::Ratio { open: 2.3e-5, close: 2.3e-5, close_today: 2.3e-4 }, None),
    ("IC", "CFFEX", "CFX", 200.0, 0.2, 0.14, 0.10, Fee::Ratio { open: 2.3e-5, close: 2.3e-5, close_today: 2.3e-4 }, None),
    ("IM", "CFFEX", "CFX", 200.0, 0.2, 0.14, 0.10, Fee::Ratio { open: 2.3e-5, close: 2.3e-5, close_today: 2.3e-4 }, None),
    ("T", "CFFEX", "CFX", 10000.0, 0.005, 0.02, 0.02, Fee::PerLot { open: 3.0, close: 3.0, close_today: 0.0 }, None),
];

pub fn product_of(symbol: &str) -> Option<String> {
    let p = symbol.strip_prefix("FUT_")?.to_uppercase();
    (!p.is_empty() && p.chars().all(|c| c.is_ascii_alphabetic())).then_some(p)
}

pub fn is_futures(symbol: &str) -> bool {
    product_of(symbol).is_some()
}

pub fn spec_for(symbol: &str) -> Option<ContractSpec> {
    let p = product_of(symbol)?;
    if let Ok(f) = std::env::var("CMSQ_FUTURES_SPECS") {
        if let Some(s) = std::fs::read_to_string(f).ok().and_then(|t| serde_json::from_str::<Vec<ContractSpec>>(&t).ok()).and_then(|v| v.into_iter().find(|s| s.product == p)) {
            return Some(s);
        }
    }
    TABLE.iter().find(|r| r.0 == p).map(|r| ContractSpec {
        product: r.0.into(), exchange: r.1.into(), ts_suffix: r.2.into(), multiplier: r.3, tick: r.4, margin_rate: r.5, limit_pct: r.6, fee: r.7,
        night: r.8.map(|(a, b)| (a.to_string(), b.to_string())), close_order: None,
    })
}

pub fn products() -> Vec<&'static str> {
    TABLE.iter().map(|r| r.0).collect()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct FuturesCosts {
    /// fill slippage in ticks per side
    pub slippage_ticks: f64,
    /// notional exposure of position 1.0, as a multiple of equity
    pub leverage: f64,
}
impl Default for FuturesCosts {
    fn default() -> Self {
        Self { slippage_ticks: 1.0, leverage: 1.0 }
    }
}

fn fee(spec: &ContractSpec, lots: f64, px: f64, kind: u8) -> f64 {
    let lots = lots.abs();
    match spec.fee {
        Fee::PerLot { open, close, close_today } => lots * [open, close, close_today][kind as usize],
        Fee::Ratio { open, close, close_today } => lots * px * spec.multiplier * [open, close, close_today][kind as usize],
    }
}

fn round_tick(px: f64, tick: f64) -> f64 {
    (px / tick).round() * tick
}

/// Simulate target positions `pos` ∈ [-1, 1] on a main-contract series of
/// DAILY bars (trading day = bar date). `costs.multiplier` scales fees and
/// slippage (cost stress).
pub fn simulate(spec: &ContractSpec, bars: &[Bar], pos: &[f64], fx: &FuturesCosts, costs: &CostModel) -> SimResult {
    let days: Vec<String> = bars.iter().map(|b| b.date.get(..10).unwrap_or(&b.date).to_string()).collect();
    simulate_session(spec, bars, &days, pos, fx, costs)
}

/// Like [`simulate`] for bars of any granularity: `trading_days[i]` is the
/// trading day bar i belongs to (night-session bars carry the NEXT trading
/// day, see [`trading_day_for`]). Several bars may share a trading day; a lot
/// opened and closed within one trading day pays the close-today fee tier,
/// with the exchange's close order ([`CloseOrder`]) deciding which lots a
/// close consumes. Price limits use the last close of the previous trading day.
pub fn simulate_session(spec: &ContractSpec, bars: &[Bar], trading_days: &[String], pos: &[f64], fx: &FuturesCosts, costs: &CostModel) -> SimResult {
    assert_eq!(bars.len(), pos.len());
    assert_eq!(bars.len(), trading_days.len());
    let n = bars.len();
    let mut r = SimResult { equity: vec![INITIAL_CAPITAL; n], exposure: vec![0.0; n], ..Default::default() };
    if n == 0 {
        return r;
    }
    let m = costs.multiplier;
    let slip = spec.tick * fx.slippage_ticks * m;
    let mult = spec.multiplier;
    let order = spec.close_order();
    let today_cheaper = match spec.fee {
        Fee::PerLot { close, close_today, .. } | Fee::Ratio { close, close_today, .. } => close_today < close,
    };
    let mut equity = INITIAL_CAPITAL;
    let mut lots = 0.0_f64; // signed
    let mut ledger = Ledger::default();
    // previous trading day's last close and its adj factor (limit reference)
    let (mut ref_close, mut ref_adj) = (bars[0].close, bars[0].adj_factor);
    let mut day_close = (bars[0].close, bars[0].adj_factor);
    let fac = |b: &Bar| b.adj_factor.unwrap_or(1.0);
    for t in 1..n {
        let (b, pb) = (&bars[t], &bars[t - 1]);
        let day = trading_days[t].as_str();
        if trading_days[t] != trading_days[t - 1] {
            (ref_close, ref_adj) = day_close;
        }
        let rolled = matches!((b.adj_factor, pb.adj_factor), (Some(a1), Some(a0)) if a0 > 0.0 && (a1 / a0 - 1.0).abs() > 1e-12);
        // previous close in the CURRENT contract's terms (ratio continuity on roll days)
        let prev_close = match (b.adj_factor, pb.adj_factor) {
            (Some(a1), Some(a0)) if a1 > 0.0 => pb.close * a0 / a1,
            _ => pb.close,
        };
        // overnight / bar-to-bar move on held lots (incl. the night session)
        equity += lots * mult * (b.open - prev_close);
        if rolled && lots != 0.0 {
            // close old + open new at the roll-day open: both legs pay fees and slippage
            let (his, tod) = ledger.close(day, lots.abs(), order, today_cheaper);
            let c = (fee(spec, his, b.open, 1) + fee(spec, tod, b.open, 2) + fee(spec, lots, b.open, 0)) * m + 2.0 * lots.abs() * slip * mult;
            r.close_today_lots += tod;
            r.close_today_fees += fee(spec, tod, b.open, 2) * m;
            equity -= c;
            r.costs_paid += c;
            r.trades += 2;
            ledger = Ledger::default();
            ledger.open(day, lots.abs());
            r.fills.push(Fill { t, date: b.date.clone(), qty: -lots, price: b.open - lots.signum() * slip, adj_price: (b.open - lots.signum() * slip) * fac(b), cost: c / 2.0, kind: "roll_close".into() });
            r.fills.push(Fill { t, date: b.date.clone(), qty: lots, price: b.open + lots.signum() * slip, adj_price: (b.open + lots.signum() * slip) * fac(b), cost: c / 2.0, kind: "roll_open".into() });
        }
        let ref_cur = match (b.adj_factor, ref_adj) {
            (Some(a1), Some(a0)) if a1 > 0.0 => ref_close * a0 / a1,
            _ => ref_close,
        };
        let up = round_tick(ref_cur * (1.0 + spec.limit_pct), spec.tick);
        let down = round_tick(ref_cur * (1.0 - spec.limit_pct), spec.tick);
        let target = pos[t - 1].clamp(-1.0, 1.0);
        let suspended = b.volume <= 0.0;
        let notional_per_lot = b.open * mult;
        let mut want = (target * equity.max(0.0) * fx.leverage / notional_per_lot).trunc();
        // margin cap: required margin may not exceed equity
        let max_lots = (equity.max(0.0) / (notional_per_lot * spec.margin_rate)).floor();
        want = want.clamp(-max_lots, max_lots);
        let delta = want - lots;
        if delta != 0.0 {
            if suspended {
                r.blocked_suspended += 1;
            } else if delta > 0.0 && b.open >= up - spec.tick * 0.5 {
                r.blocked_limit_up += 1;
            } else if delta < 0.0 && b.open <= down + spec.tick * 0.5 {
                r.blocked_limit_down += 1;
            } else {
                // closing part (reduces |lots|) pays the close or close-today fee, the rest the open fee
                let closing = if lots.signum() != delta.signum() && lots != 0.0 { delta.abs().min(lots.abs()) } else { 0.0 };
                let opening = delta.abs() - closing;
                let (his, tod) = if closing > 0.0 { ledger.close(day, closing, order, today_cheaper) } else { (0.0, 0.0) };
                let fc = fee(spec, his, b.open, 1) * m;
                let ft = fee(spec, tod, b.open, 2) * m;
                let fo = fee(spec, opening, b.open, 0) * m;
                let px = b.open + delta.signum() * slip;
                if closing > 0.0 {
                    let cc = fc + ft + closing * slip * mult;
                    r.fills.push(Fill { t, date: b.date.clone(), qty: closing * delta.signum(), price: px, adj_price: px * fac(b), cost: cc,
                        kind: if tod > 0.0 && his == 0.0 { "close_today".into() } else if tod > 0.0 { "close+close_today".into() } else { "close".into() } });
                }
                if opening > 0.0 {
                    if ledger.total() <= 1e-9 {
                        ledger = Ledger::default();
                    }
                    ledger.open(day, opening);
                    r.fills.push(Fill { t, date: b.date.clone(), qty: opening * delta.signum(), price: px, adj_price: px * fac(b), cost: fo + opening * slip * mult, kind: "open".into() });
                }
                r.close_today_lots += tod;
                r.close_today_fees += ft;
                let c = fc + ft + fo;
                equity -= c + delta.abs() * slip * mult;
                r.costs_paid += c + delta.abs() * slip * mult;
                r.trades += 1;
                lots = want;
            }
        }
        // intraday mark from the open (slippage was charged as a cost above)
        equity += lots * mult * (b.close - b.open);
        r.equity[t] = equity;
        r.exposure[t] = if equity > 0.0 { lots * b.close * mult / equity } else { 0.0 };
        day_close = (b.close, b.adj_factor);
        if equity <= 0.0 {
            // account blown: flat from here on (no fill recorded: there is no price to close at)
            lots = 0.0;
            ledger = Ledger::default();
        }
    }
    r.final_units = lots;
    r.final_price = bars[n - 1].close;
    r
}

/// One daily bar of a single contract (tushare fut_daily).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FutBar {
    pub contract: String,
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub settle: Option<f64>,
    pub volume: f64,
    pub oi: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Roll {
    pub date: String,
    pub from: String,
    pub to: String,
    pub factor: f64,
}

/// Main-contract continuous series. The main contract switches to the one
/// with the largest open interest once it has led for `confirm_days`
/// consecutive closes; the decision uses data up to close t and takes effect on
/// t+1 (no look-ahead), and never rolls back to an earlier-expiring contract.
/// Contract codes must sort by expiry (RB2501 < RB2505).
pub fn build_main_series(all: &[FutBar], confirm_days: usize) -> (Vec<Bar>, Vec<Roll>, Vec<String>) {
    use std::collections::BTreeMap;
    let mut by_date: BTreeMap<&str, Vec<&FutBar>> = BTreeMap::new();
    for b in all {
        by_date.entry(b.date.as_str()).or_default().push(b);
    }
    let mut out = vec![];
    let mut rolls = vec![];
    let mut held_contract = vec![];
    let mut cur: Option<String> = None;
    let mut leader: Option<(String, usize)> = None;
    let mut factor = 1.0;
    let mut pending: Option<String> = None;
    let mut prev_day: Option<Vec<&FutBar>> = None;
    for (d, day) in &by_date {
        // apply yesterday's roll decision at today's open
        if let Some(next) = pending.take() {
            let pd = prev_day.as_ref().unwrap();
            let old = pd.iter().find(|x| Some(&x.contract) == cur.as_ref());
            let new = pd.iter().find(|x| x.contract == next);
            if let (Some(o), Some(nw), true) = (old, new, day.iter().any(|x| x.contract == next)) {
                if nw.close > 0.0 {
                    factor *= o.close / nw.close;
                }
                rolls.push(Roll { date: d.to_string(), from: cur.clone().unwrap(), to: next.clone(), factor });
                cur = Some(next);
            }
        }
        if cur.is_none() {
            cur = day.iter().max_by(|a, b| a.oi.partial_cmp(&b.oi).unwrap()).map(|x| x.contract.clone());
        }
        let Some(c) = cur.clone() else { continue };
        if let Some(b) = day.iter().find(|x| x.contract == c) {
            out.push(Bar { date: d.to_string(), open: b.open, high: b.high, low: b.low, close: b.close, volume: b.volume, adj_factor: Some(factor) });
            held_contract.push(c.clone());
        } else if let Some(last) = out.last().cloned() {
            // held contract has no bar today (expired early / missing): carry as no-trade day
            out.push(Bar { date: d.to_string(), open: last.close, high: last.close, low: last.close, close: last.close, volume: 0.0, adj_factor: Some(factor) });
            held_contract.push(c.clone());
        }
        // decide (at the close) whether tomorrow rolls
        if let Some(top) = day.iter().filter(|x| x.contract > c).max_by(|a, b| a.oi.partial_cmp(&b.oi).unwrap()) {
            let cur_oi = day.iter().find(|x| x.contract == c).map(|x| x.oi).unwrap_or(0.0);
            if top.oi > cur_oi {
                leader = match leader.take() {
                    Some((l, k)) if l == top.contract => Some((l, k + 1)),
                    _ => Some((top.contract.clone(), 1)),
                };
                if leader.as_ref().is_some_and(|(_, k)| *k >= confirm_days.max(1)) {
                    pending = Some(top.contract.clone());
                    leader = None;
                }
            } else {
                leader = None;
            }
        }
        prev_day = Some(day.clone());
    }
    (out, rolls, held_contract)
}

/// Trading day a timestamp belongs to. Night session (≥ 20:00) and the
/// after-midnight part (< 03:00) belong to the NEXT trading day; Friday night
/// and Saturday early morning belong to Monday. `is_open` tells exchange
/// holidays (pass `|_| true` for weekdays only).
pub fn trading_day_for(ts: chrono::NaiveDateTime, is_open: impl Fn(chrono::NaiveDate) -> bool) -> chrono::NaiveDate {
    use chrono::{Datelike, Duration, Timelike, Weekday};
    let t = ts.time();
    let mut d = ts.date();
    let next_open = |mut x: chrono::NaiveDate| {
        loop {
            x += Duration::days(1);
            if !matches!(x.weekday(), Weekday::Sat | Weekday::Sun) && is_open(x) {
                return x;
            }
        }
    };
    if t.hour() >= 20 {
        return next_open(d);
    }
    if t.hour() < 3 {
        // after midnight: the session started the previous evening
        d -= Duration::days(1);
        return next_open(d);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bar(d: &str, o: f64, c: f64) -> Bar {
        Bar { date: d.into(), open: o, high: o.max(c), low: o.min(c), close: c, volume: 1e5, adj_factor: Some(1.0) }
    }
    fn no_cost() -> CostModel {
        CostModel { commission: 0.0, min_commission: 0.0, slippage: 0.0, multiplier: 0.0 }
    }

    #[test]
    fn specs_and_symbols() {
        let rb = spec_for("FUT_RB").unwrap();
        assert_eq!((rb.multiplier, rb.tick, rb.exchange.as_str()), (10.0, 1.0, "SHFE"));
        assert!(spec_for("FUT_IF").unwrap().night.is_none());
        assert!(spec_for("SH600519").is_none() && spec_for("FUT_ZZ").is_none());
    }

    #[test]
    fn short_profits_when_price_falls_and_uses_multiplier() {
        let rb = spec_for("FUT_RB").unwrap();
        let b = vec![bar("2024-01-02", 4000.0, 4000.0), bar("2024-01-03", 4000.0, 3900.0), bar("2024-01-04", 3900.0, 3800.0)];
        let s = simulate(&rb, &b, &[-1.0, -1.0, -1.0], &FuturesCosts { slippage_ticks: 0.0, leverage: 1.0 }, &no_cost());
        // 1,000,000 / (4000*10) = 25 lots short; -100 then -100 per ton * 10 * 25
        assert!((s.equity[1] - (INITIAL_CAPITAL + 25.0 * 10.0 * 100.0)).abs() < 1e-6, "{:?}", s.equity);
        // re-sized at the next open: 1,025,000 / 39,000 per lot -> 26 lots
        assert!((s.equity[2] - (s.equity[1] + 26.0 * 10.0 * 100.0)).abs() < 1e-6, "{:?}", s.equity);
    }

    #[test]
    fn limit_up_open_blocks_buys_and_limit_down_blocks_sells() {
        let rb = spec_for("FUT_RB").unwrap();
        let b = vec![bar("2024-01-02", 4000.0, 4000.0), bar("2024-01-03", 4200.0, 4200.0), bar("2024-01-04", 4200.0, 4200.0)];
        let s = simulate(&rb, &b, &[1.0, 1.0, 1.0], &FuturesCosts::default(), &CostModel::default());
        assert_eq!(s.blocked_limit_up, 1);
        let b = vec![bar("2024-01-02", 4000.0, 4000.0), bar("2024-01-03", 3800.0, 3800.0)];
        let s = simulate(&rb, &b, &[-1.0, -1.0], &FuturesCosts::default(), &CostModel::default());
        assert_eq!(s.blocked_limit_down, 1);
    }

    #[test]
    fn fees_per_lot_and_ratio_and_roll_pays_both_legs() {
        let al = spec_for("FUT_AL").unwrap();
        assert_eq!(fee(&al, 3.0, 20000.0, 0), 9.0);
        let rb = spec_for("FUT_RB").unwrap();
        assert!((fee(&rb, 2.0, 4000.0, 1) - 2.0 * 4000.0 * 10.0 * 1e-4).abs() < 1e-9);
        let mut b = vec![bar("2024-01-02", 4000.0, 4000.0), bar("2024-01-03", 4000.0, 4000.0), bar("2024-01-04", 4100.0, 4100.0)];
        let flat = simulate(&rb, &b, &[1.0; 3], &FuturesCosts { slippage_ticks: 0.0, leverage: 1.0 }, &CostModel::default());
        b[2].adj_factor = Some(4000.0 / 4100.0); // roll into a contract trading 100 higher
        let rolled = simulate(&rb, &b, &[1.0; 3], &FuturesCosts { slippage_ticks: 0.0, leverage: 1.0 }, &CostModel::default());
        assert!(rolled.trades > flat.trades);
        // no fake P&L from the price jump between contracts
        assert!(rolled.equity[2] < flat.equity[2] && (flat.equity[1] - rolled.equity[2]).abs() < 300.0, "{:?} {:?}", flat.equity, rolled.equity);
    }

    #[test]
    fn margin_caps_leverage() {
        let ifs = spec_for("FUT_IF").unwrap();
        let b = vec![bar("2024-01-02", 3500.0, 3500.0), bar("2024-01-03", 3500.0, 3500.0)];
        let s = simulate(&ifs, &b, &[1.0, 1.0], &FuturesCosts { slippage_ticks: 0.0, leverage: 50.0 }, &no_cost());
        // margin 12%: at most floor(1e6 / (3500*300*0.12)) = 7 lots => exposure ≈ 7*3500*300/1e6
        assert!((s.exposure[1] - 7.0 * 3500.0 * 300.0 / s.equity[1]).abs() < 1e-6, "{}", s.exposure[1]);
    }

    #[test]
    fn main_series_rolls_on_confirmed_oi_lead_without_lookahead() {
        let mk = |c: &str, d: &str, px: f64, oi: f64| FutBar { contract: c.into(), date: d.into(), open: px, high: px, low: px, close: px, settle: None, volume: 1e4, oi };
        let days = ["2024-01-02", "2024-01-03", "2024-01-04", "2024-01-05", "2024-01-08"];
        let mut v = vec![];
        for (i, d) in days.iter().enumerate() {
            v.push(mk("RB2405", d, 4000.0, 100.0 - 20.0 * i as f64));
            v.push(mk("RB2410", d, 4100.0, 50.0 + 30.0 * i as f64));
        }
        let (bars, rolls, held) = build_main_series(&v, 2);
        // 2410 leads from day 2 (index 2: 110 vs 60), confirmed after 2 closes (index 3) -> roll at index 4
        assert_eq!(rolls.len(), 1, "{rolls:?}");
        assert_eq!(rolls[0].date, "2024-01-08");
        assert_eq!(held, vec!["RB2405", "RB2405", "RB2405", "RB2405", "RB2410"]);
        assert!((bars[4].adj_factor.unwrap() - 4000.0 / 4100.0).abs() < 1e-12);
        assert_eq!(bars[4].close, 4100.0, "raw price of the held contract");
    }

    #[test]
    fn night_session_belongs_to_next_trading_day() {
        use chrono::NaiveDate;
        let dt = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        let open = |_d: NaiveDate| true;
        assert_eq!(trading_day_for(dt("2024-01-03 21:05:00"), open), NaiveDate::from_ymd_opt(2024, 1, 4).unwrap());
        assert_eq!(trading_day_for(dt("2024-01-04 00:30:00"), open), NaiveDate::from_ymd_opt(2024, 1, 4).unwrap());
        // Friday night -> Monday
        assert_eq!(trading_day_for(dt("2024-01-05 22:00:00"), open), NaiveDate::from_ymd_opt(2024, 1, 8).unwrap());
        assert_eq!(trading_day_for(dt("2024-01-06 01:00:00"), open), NaiveDate::from_ymd_opt(2024, 1, 8).unwrap());
        assert_eq!(trading_day_for(dt("2024-01-04 10:00:00"), open), NaiveDate::from_ymd_opt(2024, 1, 4).unwrap());
        // holiday Monday skipped
        let hol = |d: NaiveDate| d != NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();
        assert_eq!(trading_day_for(dt("2024-01-05 22:00:00"), hol), NaiveDate::from_ymd_opt(2024, 1, 9).unwrap());
    }

    fn zero_slip() -> FuturesCosts {
        FuturesCosts { slippage_ticks: 0.0, leverage: 1.0 }
    }

    #[test]
    fn daily_bars_never_pay_close_today() {
        let ma = spec_for("FUT_MA").unwrap();
        let b: Vec<Bar> = (0..30).map(|i| bar(&format!("2024-02-{:02}", i + 1), 2500.0, 2500.0)).collect();
        let pos: Vec<f64> = (0..30).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let s = simulate(&ma, &b, &pos, &zero_slip(), &CostModel::default());
        assert!(s.trades > 20 && s.close_today_lots == 0.0 && s.close_today_fees == 0.0, "{} {}", s.trades, s.close_today_lots);
        assert!(s.fills.iter().all(|f| f.kind != "close_today"));
    }

    #[test]
    fn intraday_round_trip_pays_close_today_tier() {
        // MA (CZCE): open 2 / close 2 / close-today 6 yuan per lot
        let ma = spec_for("FUT_MA").unwrap();
        let b = vec![bar("2024-02-01 09:00", 2500.0, 2500.0), bar("2024-02-01 09:01", 2500.0, 2500.0), bar("2024-02-01 09:02", 2500.0, 2500.0), bar("2024-02-02 09:00", 2500.0, 2500.0)];
        let td = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // open on bar 1, close on bar 2 (same trading day)
        let s = simulate_session(&ma, &b, &td(&["2024-02-01"; 4]), &[1.0, 0.0, 0.0, 0.0], &zero_slip(), &CostModel::default());
        let lots = (INITIAL_CAPITAL / (2500.0 * 10.0)).trunc();
        assert_eq!(s.close_today_lots, lots);
        assert!((s.close_today_fees - 6.0 * lots).abs() < 1e-9 && (s.costs_paid - 8.0 * lots).abs() < 1e-9, "{s:?}");
        // the same orders across a trading-day boundary pay the ordinary close fee
        let s2 = simulate_session(&ma, &b, &td(&["2024-02-01", "2024-02-01", "2024-02-02", "2024-02-02"]), &[1.0, 0.0, 0.0, 0.0], &zero_slip(), &CostModel::default());
        assert_eq!(s2.close_today_lots, 0.0);
        assert!((s2.costs_paid - 4.0 * lots).abs() < 1e-9);
        // a night-session open (21:00, trading day = next day) closed next morning IS close-today
        let s3 = simulate_session(&ma, &b, &td(&["2024-02-01", "2024-02-02", "2024-02-02", "2024-02-02"]), &[1.0, 0.0, 0.0, 0.0], &zero_slip(), &CostModel::default());
        assert_eq!(s3.close_today_lots, lots);
    }

    #[test]
    fn close_order_decides_which_lots_are_closed() {
        let mut l = Ledger::default();
        l.open("d1", 3.0);
        l.open("d2", 2.0);
        assert_eq!(l.clone().close("d2", 4.0, CloseOrder::Fifo, true), (3.0, 1.0));
        assert_eq!(l.clone().close("d2", 4.0, CloseOrder::TodayFirst, false), (2.0, 2.0));
        assert_eq!(l.clone().close("d2", 1.0, CloseOrder::Cheapest, true), (0.0, 1.0));
        assert_eq!(l.clone().close("d2", 1.0, CloseOrder::Cheapest, false), (1.0, 0.0));
        assert_eq!(l.close("d2", 99.0, CloseOrder::Fifo, false), (3.0, 2.0), "never closes more than held");
        assert_eq!(spec_for("FUT_IF").unwrap().close_order(), CloseOrder::TodayFirst);
        assert_eq!(spec_for("FUT_RB").unwrap().close_order(), CloseOrder::Cheapest);
        assert_eq!(spec_for("FUT_M").unwrap().close_order(), CloseOrder::Fifo);
    }

    #[test]
    fn cffex_partial_reduce_hits_close_today_first() {
        // IF close-today 10x the close fee: add on day 2 then cut back same day -> close-today
        let ifs = spec_for("FUT_IF").unwrap();
        let b = vec![bar("d1a", 3500.0, 3500.0), bar("d1b", 3500.0, 3500.0), bar("d2a", 3500.0, 3500.0), bar("d2b", 3500.0, 3500.0)];
        let td: Vec<String> = ["d1", "d1", "d2", "d2"].iter().map(|x| x.to_string()).collect();
        let s = simulate_session(&ifs, &b, &td, &[0.5, 1.0, 0.5, 0.5], &FuturesCosts { slippage_ticks: 0.0, leverage: 3.0 }, &CostModel::default());
        assert!(s.close_today_lots > 0.0, "{:?}", s.fills);
        assert!(s.fills.iter().any(|f| f.kind == "close_today"));
    }
}
