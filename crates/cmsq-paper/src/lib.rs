//! cmsq-paper: host-side, trusted paper-trading matcher.
//!
//! Simulated fills against INCOMING EXCHANGE QUOTES (CTP ticks / A-share quotes).
//! There is no order book among participants and no path to any broker: this crate
//! has no network, CTP or broker dependency (enforced by `tests/no_order_path.rs`).
//!
//! Matching (per working order, per quote, in order-id order):
//! * fill size per quote ≤ floor(Δvolume × participation) (and ≤ displayed L1 size
//!   when the quote carries it); Δvolume = 0 ⇒ no fill.
//! * market: buy at ask1 (sell at bid1), else at last when no L1.
//! * limit, marketable (ask1 ≤ L for a buy): fill at ask1.
//! * limit, passive: [`LimitFill::TradeThrough`] (default, conservative) needs
//!   last strictly better than L; [`LimitFill::TouchQueue`] also fills on a touch
//!   once the traded volume at L has consumed the estimated queue ahead
//!   (bid/ask size at L when the order was placed).
//! * no buy fills at/above limit-up, no sell fills at/below limit-down; suspended
//!   instruments reject orders and never fill.
//! * A-shares: 100-share buy lots (odd lots only to sell the whole holding), T+1,
//!   commission max(min, rate×value) per order, dated stamp duty (cmsq-core).
//! * futures: multiplier, margin, per-lot/ratio fees with close-today tier, the
//!   exchange's close-order rule, daily mark-to-market at the settlement price.
//! * orders are day orders: they expire at the trading-day rollover, which is
//!   taken from the quote's trading_day (night sessions via cmsq-core calendars).
//!
//! Only strategies in status `Promoted` may submit orders.
pub mod driver;
pub mod session;
pub use driver::SignalDriver;
pub use session::{verify_chain, PaperSession};

use cmsq_core::futures::{CloseOrder, ContractSpec, Fee};
use cmsq_core::models::{Strategy, StrategyStatus};
use cmsq_core::scorer::{self, CostModel, Rules};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Offset {
    Open,
    /// close, using the exchange's close-order rule
    Close,
    /// SHFE/INE explicit 平今
    CloseToday,
    /// SHFE/INE explicit 平昨
    CloseYesterday,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrderKind {
    Market,
    Limit { price: f64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LimitFill {
    /// passive limit fills only when the market trades strictly through the limit
    #[default]
    TradeThrough,
    /// also on a touch, after the estimated queue ahead has traded
    TouchQueue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaperConfig {
    pub initial_cash: f64,
    /// fraction of each quote's traded volume we may take
    pub participation: f64,
    pub limit_fill: LimitFill,
    /// A-share commission (slippage/multiplier fields unused: fills are at quotes)
    pub ashare_costs: CostModel,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            initial_cash: scorer::INITIAL_CAPITAL,
            participation: 0.1,
            limit_fill: LimitFill::TradeThrough,
            ashare_costs: CostModel::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Market {
    AShare { rules: Rules },
    Futures { spec: ContractSpec },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstrumentSetup {
    pub symbol: String,
    pub market: Market,
    /// previous close (A-share) / previous settlement (futures): price-limit reference
    pub prev_close: f64,
    #[serde(default)]
    pub suspended: bool,
    /// exchange-published limits by trading day (YYYY-MM-DD -> (up, down)), e.g. tushare
    /// stk_limit / ft_limit. Used when the quote itself carries no limits (stored ticks,
    /// replayed bars); takes precedence over the rule-derived band.
    #[serde(default)]
    pub exchange_limits: std::collections::BTreeMap<String, (f64, f64)>,
}

/// One incoming quote. `cum_volume` is the trading day's cumulative volume
/// (shares for A-shares, lots for futures), as CTP reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Quote {
    pub symbol: String,
    pub ts: String,
    /// YYYY-MM-DD
    pub trading_day: String,
    pub last: f64,
    pub cum_volume: f64,
    #[serde(default)]
    pub bid1: Option<f64>,
    #[serde(default)]
    pub ask1: Option<f64>,
    #[serde(default)]
    pub bid_vol1: Option<f64>,
    #[serde(default)]
    pub ask_vol1: Option<f64>,
    #[serde(default)]
    pub upper_limit: Option<f64>,
    #[serde(default)]
    pub lower_limit: Option<f64>,
}

impl From<&cmsq_core::ctp::Tick> for Quote {
    fn from(t: &cmsq_core::ctp::Tick) -> Self {
        Quote {
            symbol: t.instrument.clone(),
            ts: t.ts.clone(),
            trading_day: t.trading_day.clone(),
            last: t.last,
            cum_volume: t.volume,
            bid1: t.bid1.filter(|x| *x > 0.0 && x.is_finite()),
            ask1: t.ask1.filter(|x| *x > 0.0 && x.is_finite()),
            bid_vol1: None,
            ask_vol1: None,
            upper_limit: None,
            lower_limit: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Working,
    PartFilled,
    Filled,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Order {
    pub id: u64,
    pub strategy_id: String,
    pub symbol: String,
    pub side: Side,
    pub offset: Offset,
    pub kind: OrderKind,
    pub qty: i64,
    pub filled: i64,
    pub status: OrderStatus,
    pub ts: String,
    /// cash (A-share buy) or margin+fee (futures open) still frozen for the unfilled part
    pub frozen: f64,
    /// TouchQueue estimate of volume ahead at the limit price
    pub queue_ahead: f64,
    pub commission_paid: f64,
    /// Σ price × qty filled so far
    pub filled_value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fill {
    pub order_id: u64,
    pub symbol: String,
    pub side: Side,
    pub offset: Offset,
    pub price: f64,
    pub qty: i64,
    pub fee: f64,
    /// futures: lots charged at the close-today tier
    pub close_today_lots: i64,
    pub realized: f64,
    pub ts: String,
    pub trading_day: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub long_today: i64,
    pub long_yd: i64,
    pub short_today: i64,
    pub short_yd: i64,
    pub long_cost: f64,
    pub short_cost: f64,
    /// lots/shares reserved by working close/sell orders
    pub long_frozen: i64,
    pub short_frozen: i64,
    /// non-SHFE close-today fee tier counters for the current day
    pub long_ct_charged: i64,
    pub short_ct_charged: i64,
    /// lots opened during the current trading day
    pub long_opened_today: i64,
    pub short_opened_today: i64,
}

impl Position {
    pub fn long(&self) -> i64 {
        self.long_today + self.long_yd
    }
    pub fn short(&self) -> i64 {
        self.short_today + self.short_yd
    }
    pub fn net(&self) -> i64 {
        self.long() - self.short()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub initial_cash: f64,
    /// cash balance (futures: includes margin in use)
    pub cash: f64,
    pub frozen: f64,
    pub margin: f64,
    pub fees: f64,
    pub realized: f64,
    pub trading_day: Option<String>,
    pub positions: BTreeMap<String, Position>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Accepted {
        order: Order,
    },
    Rejected {
        strategy_id: String,
        symbol: String,
        side: Side,
        qty: i64,
        reason: String,
        ts: String,
    },
    Filled {
        fill: Fill,
    },
    Cancelled {
        order_id: u64,
        reason: String,
    },
    Settled {
        trading_day: String,
        prices: BTreeMap<String, f64>,
        cash: f64,
        equity: f64,
        margin: f64,
    },
}

#[derive(Debug, thiserror::Error, PartialEq)]
#[error("{0}")]
pub struct Reject(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Inst {
    setup: InstrumentSetup,
    last_cum: Option<f64>,
    last: Option<Quote>,
    settle: Option<f64>,
}

/// Engine state. Serialisable for snapshots; the event log itself is not part
/// of a snapshot (it is persisted separately) but its chained hash is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperEngine {
    pub cfg: PaperConfig,
    inst: BTreeMap<String, Inst>,
    orders: BTreeMap<u64, Order>,
    next_id: u64,
    pub account: Account,
    /// events not yet taken by [`PaperEngine::take_events`]
    #[serde(skip)]
    events: Vec<Event>,
    /// number of events ever emitted
    pub event_seq: u64,
    /// sha256 chain over every event ever emitted: h_n = sha256(h_{n-1} ‖ json(e_n))
    pub event_chain: String,
    /// every fill (QIFI `trades`)
    pub fills: Vec<Fill>,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
fn round_tick(x: f64, t: f64) -> f64 {
    if t > 0.0 {
        (x / t).round() * t
    } else {
        x
    }
}
fn fee_rates(f: &Fee) -> (f64, f64, f64, bool) {
    match *f {
        Fee::PerLot {
            open,
            close,
            close_today,
        } => (open, close, close_today, true),
        Fee::Ratio {
            open,
            close,
            close_today,
        } => (open, close, close_today, false),
    }
}

impl PaperEngine {
    pub fn new(cfg: PaperConfig) -> Self {
        let account = Account {
            initial_cash: cfg.initial_cash,
            cash: cfg.initial_cash,
            frozen: 0.0,
            margin: 0.0,
            fees: 0.0,
            realized: 0.0,
            trading_day: None,
            positions: BTreeMap::new(),
        };
        Self {
            cfg,
            inst: BTreeMap::new(),
            orders: BTreeMap::new(),
            next_id: 1,
            account,
            events: vec![],
            event_seq: 0,
            event_chain: String::new(),
            fills: vec![],
        }
    }

    pub fn add_instrument(&mut self, s: InstrumentSetup) {
        self.inst.insert(
            s.symbol.clone(),
            Inst {
                setup: s,
                last_cum: None,
                last: None,
                settle: None,
            },
        );
    }
    pub fn set_suspended(&mut self, symbol: &str, on: bool) {
        if let Some(i) = self.inst.get_mut(symbol) {
            i.setup.suspended = on;
        }
    }
    /// Exchange settlement price for the current day (futures); used at rollover instead of last.
    pub fn set_settlement(&mut self, symbol: &str, px: f64) {
        if let Some(i) = self.inst.get_mut(symbol) {
            i.settle = Some(px);
        }
    }
    pub fn market(&self, symbol: &str) -> Option<&Market> {
        self.inst.get(symbol).map(|i| &i.setup.market)
    }
    pub fn events(&self) -> &[Event] {
        &self.events
    }
    pub fn orders(&self) -> impl Iterator<Item = &Order> {
        self.orders.values()
    }
    pub fn order(&self, id: u64) -> Option<&Order> {
        self.orders.get(&id)
    }

    /// Chained sha256 over every event: identical inputs ⇒ identical hash.
    pub fn event_log_hash(&self) -> String {
        self.event_chain.clone()
    }
    /// Drain pending events (for persistence), with their sequence numbers (1-based).
    pub fn take_events(&mut self) -> Vec<(u64, Event)> {
        let first = self.event_seq + 1 - self.events.len() as u64;
        std::mem::take(&mut self.events)
            .into_iter()
            .enumerate()
            .map(|(i, e)| (first + i as u64, e))
            .collect()
    }
    /// Chain step, exposed so persisted logs can be re-verified.
    pub fn chain_step(prev: &str, e: &Event) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(prev.as_bytes());
        h.update(serde_json::to_vec(e).unwrap());
        hex::encode(h.finalize())
    }
    fn push(&mut self, e: Event) {
        self.event_chain = Self::chain_step(&self.event_chain, &e);
        self.event_seq += 1;
        self.events.push(e);
    }

    fn mark(&self, sym: &str) -> Option<f64> {
        self.inst
            .get(sym)
            .and_then(|i| i.last.as_ref().map(|q| q.last))
            .or_else(|| self.inst.get(sym).map(|i| i.setup.prev_close))
    }

    /// Mark-to-market equity at the latest quotes.
    pub fn equity(&self) -> f64 {
        let mut eq = self.account.cash;
        for (sym, p) in &self.account.positions {
            let Some(i) = self.inst.get(sym) else {
                continue;
            };
            let m = self.mark(sym).unwrap_or(0.0);
            match &i.setup.market {
                Market::AShare { .. } => eq += p.long() as f64 * m,
                Market::Futures { spec } => {
                    eq += (m - p.long_cost) * spec.multiplier * p.long() as f64;
                    eq += (p.short_cost - m) * spec.multiplier * p.short() as f64;
                }
            }
        }
        eq
    }
    pub fn available(&self) -> f64 {
        self.account.cash - self.account.frozen - self.account.margin
    }

    /// (up, down) price band in force for `sym` at quote `q` (quote > exchange data > rules)
    pub fn price_band(&self, sym: &str, q: Option<&Quote>) -> (Option<f64>, Option<f64>) {
        self.limits(sym, q)
    }

    fn limits(&self, sym: &str, q: Option<&Quote>) -> (Option<f64>, Option<f64>) {
        let i = &self.inst[sym];
        let (qu, ql) = q
            .map(|q| (q.upper_limit, q.lower_limit))
            .unwrap_or((None, None));
        let pc = i.setup.prev_close;
        let day = q
            .map(|q| q.trading_day.clone())
            .or_else(|| self.account.trading_day.clone())
            .unwrap_or_default();
        let (cu, cl) = match &i.setup.market {
            Market::AShare { rules } => match scorer::limit_for(sym, &day, rules, None) {
                Some(p) => (Some(round2(pc * (1.0 + p))), Some(round2(pc * (1.0 - p)))),
                None => (None, None),
            },
            Market::Futures { spec } => (
                Some(round_tick(pc * (1.0 + spec.limit_pct), spec.tick)),
                Some(round_tick(pc * (1.0 - spec.limit_pct), spec.tick)),
            ),
        };
        let (eu, el) = match i.setup.exchange_limits.get(&day) {
            Some(&(u, l)) => (Some(u), Some(l)),
            None => (None, None),
        };
        (qu.or(eu).or(cu), ql.or(el).or(cl))
    }

    fn reject(
        &mut self,
        s: &Strategy,
        sym: &str,
        side: Side,
        qty: i64,
        ts: &str,
        why: impl Into<String>,
    ) -> Reject {
        let reason = why.into();
        self.push(Event::Rejected {
            strategy_id: s.id.clone(),
            symbol: sym.into(),
            side,
            qty,
            reason: reason.clone(),
            ts: ts.into(),
        });
        Reject(reason)
    }

    /// Submit a simulated order. Only `Promoted` strategies may trade.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        s: &Strategy,
        symbol: &str,
        side: Side,
        offset: Offset,
        kind: OrderKind,
        qty: i64,
        ts: &str,
    ) -> Result<u64, Reject> {
        if s.status != StrategyStatus::Promoted {
            return Err(self.reject(
                s,
                symbol,
                side,
                qty,
                ts,
                format!(
                    "strategy {} is {}, only Promoted strategies may paper-trade",
                    s.id, s.status
                ),
            ));
        }
        let Some(i) = self.inst.get(symbol).cloned() else {
            return Err(self.reject(s, symbol, side, qty, ts, "unknown instrument"));
        };
        if i.setup.suspended {
            return Err(self.reject(s, symbol, side, qty, ts, "suspended"));
        }
        if qty <= 0 {
            return Err(self.reject(s, symbol, side, qty, ts, "qty must be positive"));
        }
        if let OrderKind::Limit { price } = kind {
            if !(price.is_finite() && price > 0.0) {
                return Err(self.reject(s, symbol, side, qty, ts, "bad limit price"));
            }
            let (up, dn) = self.limits(symbol, i.last.as_ref());
            if up.is_some_and(|u| price > u + EPS) || dn.is_some_and(|d| price < d - EPS) {
                return Err(self.reject(
                    s,
                    symbol,
                    side,
                    qty,
                    ts,
                    "limit price outside the daily price band",
                ));
            }
        }
        let pos = self
            .account
            .positions
            .get(symbol)
            .cloned()
            .unwrap_or_default();
        let (up, _) = self.limits(symbol, i.last.as_ref());
        let refpx = match kind {
            OrderKind::Limit { price } => price,
            OrderKind::Market => up.unwrap_or_else(|| {
                i.last
                    .as_ref()
                    .map(|q| q.last)
                    .unwrap_or(i.setup.prev_close)
                    * 1.2
            }),
        };
        let mut frozen = 0.0;
        match &i.setup.market {
            Market::AShare { .. } => match (side, offset) {
                (Side::Buy, Offset::Open) => {
                    if qty % 100 != 0 {
                        return Err(self.reject(
                            s,
                            symbol,
                            side,
                            qty,
                            ts,
                            "A-share buys must be in 100-share lots",
                        ));
                    }
                    let c = &self.cfg.ashare_costs;
                    let v = refpx * qty as f64;
                    frozen = v + (v * c.commission).max(c.min_commission) + v * 0.01;
                }
                (Side::Sell, Offset::Close) => {
                    let free = pos.long_yd - pos.long_frozen;
                    if qty > free {
                        return Err(self.reject(
                            s,
                            symbol,
                            side,
                            qty,
                            ts,
                            format!("T+1: only {free} shares sellable"),
                        ));
                    }
                    if qty % 100 != 0 && qty != free {
                        return Err(self.reject(
                            s,
                            symbol,
                            side,
                            qty,
                            ts,
                            "odd lots may only be sold as the whole holding",
                        ));
                    }
                }
                _ => {
                    return Err(self.reject(
                        s,
                        symbol,
                        side,
                        qty,
                        ts,
                        "A-shares: buy=open, sell=close (no shorting)",
                    ))
                }
            },
            Market::Futures { spec } => match offset {
                Offset::Open => {
                    let (fo, _, _, per_lot) = fee_rates(&spec.fee);
                    let n = refpx * spec.multiplier * qty as f64;
                    frozen = n * spec.margin_rate + if per_lot { fo * qty as f64 } else { fo * n };
                }
                o => {
                    let shfe = spec.close_order() == CloseOrder::TraderChooses;
                    if !shfe && matches!(o, Offset::CloseToday | Offset::CloseYesterday) {
                        return Err(self.reject(
                            s,
                            symbol,
                            side,
                            qty,
                            ts,
                            format!("{} has no 平今/平昨 instruction; use close", spec.exchange),
                        ));
                    }
                    let (today, yd, fz) = match side {
                        Side::Sell => (pos.long_today, pos.long_yd, pos.long_frozen),
                        Side::Buy => (pos.short_today, pos.short_yd, pos.short_frozen),
                    };
                    let avail = match o {
                        Offset::CloseToday => today,
                        Offset::CloseYesterday => yd,
                        _ => today + yd,
                    } - fz;
                    if qty > avail {
                        return Err(self.reject(
                            s,
                            symbol,
                            side,
                            qty,
                            ts,
                            format!("only {avail} lots closable"),
                        ));
                    }
                }
            },
        }
        if frozen > 0.0 && frozen > self.available() + EPS {
            return Err(self.reject(
                s,
                symbol,
                side,
                qty,
                ts,
                format!(
                    "insufficient funds: need {frozen:.2}, available {:.2}",
                    self.available()
                ),
            ));
        }
        let is_close = offset != Offset::Open;
        let p = self.account.positions.entry(symbol.into()).or_default();
        if is_close {
            match side {
                Side::Sell => p.long_frozen += qty,
                Side::Buy => p.short_frozen += qty,
            }
        }
        self.account.frozen += frozen;
        // TouchQueue: displayed size at our price is ahead of us
        let queue_ahead = match (kind, i.last.as_ref()) {
            (OrderKind::Limit { price }, Some(q)) => match side {
                Side::Buy if q.bid1.is_some_and(|b| (b - price).abs() < EPS) => {
                    q.bid_vol1.unwrap_or(f64::MAX)
                }
                Side::Sell if q.ask1.is_some_and(|a| (a - price).abs() < EPS) => {
                    q.ask_vol1.unwrap_or(f64::MAX)
                }
                Side::Buy if q.bid1.is_some_and(|b| price < b) => f64::MAX,
                Side::Sell if q.ask1.is_some_and(|a| price > a) => f64::MAX,
                _ => 0.0,
            },
            (OrderKind::Limit { .. }, None) => f64::MAX,
            _ => 0.0,
        };
        let id = self.next_id;
        self.next_id += 1;
        let o = Order {
            id,
            strategy_id: s.id.clone(),
            symbol: symbol.into(),
            side,
            offset,
            kind,
            qty,
            filled: 0,
            status: OrderStatus::Working,
            ts: ts.into(),
            frozen,
            queue_ahead,
            commission_paid: 0.0,
            filled_value: 0.0,
        };
        self.push(Event::Accepted { order: o.clone() });
        self.orders.insert(id, o);
        Ok(id)
    }

    pub fn cancel(&mut self, id: u64) -> bool {
        self.finish(id, OrderStatus::Cancelled, "cancelled")
    }

    fn finish(&mut self, id: u64, st: OrderStatus, why: &str) -> bool {
        let Some(o) = self.orders.get_mut(&id) else {
            return false;
        };
        if !matches!(o.status, OrderStatus::Working | OrderStatus::PartFilled) {
            return false;
        }
        o.status = st;
        let rest = o.qty - o.filled;
        let fz = o.frozen;
        o.frozen = 0.0;
        let (sym, side, off) = (o.symbol.clone(), o.side, o.offset);
        self.account.frozen = (self.account.frozen - fz).max(0.0);
        if off != Offset::Open {
            let p = self.account.positions.entry(sym).or_default();
            match side {
                Side::Sell => p.long_frozen -= rest,
                Side::Buy => p.short_frozen -= rest,
            }
        }
        self.push(Event::Cancelled {
            order_id: id,
            reason: why.into(),
        });
        true
    }

    /// Feed one quote; returns the fills it produced.
    pub fn on_quote(&mut self, q: &Quote) -> Vec<Fill> {
        if !self.inst.contains_key(&q.symbol) || !(q.last.is_finite() && q.last > 0.0) {
            return vec![];
        }
        if let Some(d) = self.account.trading_day.clone() {
            if q.trading_day > d {
                self.settle();
            } else if q.trading_day < d {
                return vec![]; // stale quote from a settled day
            }
        }
        self.account.trading_day = Some(q.trading_day.clone());
        let inst = self.inst.get_mut(&q.symbol).unwrap();
        let dv = match inst.last_cum {
            Some(p) if q.cum_volume >= p => q.cum_volume - p,
            Some(_) => 0.0,
            None => 0.0,
        };
        inst.last_cum = Some(q.cum_volume);
        if inst.setup.prev_close <= 0.0 {
            inst.setup.prev_close = q.last; // unknown reference: band centred on the first quote
        }
        inst.last = Some(q.clone());
        if inst.setup.suspended || dv <= 0.0 {
            return vec![];
        }
        let (up, dn) = self.limits(&q.symbol, Some(q));
        let lot = match &self.inst[&q.symbol].setup.market {
            Market::AShare { .. } => 100,
            Market::Futures { .. } => 1,
        };
        let mut budget = (dv * self.cfg.participation).floor() as i64;
        let ids: Vec<u64> = self
            .orders
            .values()
            .filter(|o| {
                o.symbol == q.symbol
                    && matches!(o.status, OrderStatus::Working | OrderStatus::PartFilled)
            })
            .map(|o| o.id)
            .collect();
        let mut fills = vec![];
        for id in ids {
            if budget <= 0 {
                break;
            }
            let o = self.orders.get(&id).unwrap().clone();
            let rest = o.qty - o.filled;
            let (px, depth) = match o.side {
                Side::Buy => (q.ask1.unwrap_or(q.last), q.ask1.and(q.ask_vol1)),
                Side::Sell => (q.bid1.unwrap_or(q.last), q.bid1.and(q.bid_vol1)),
            };
            // price-limit blocks
            if o.side == Side::Buy && up.is_some_and(|u| px >= u - EPS && q.last >= u - EPS) {
                continue;
            }
            if o.side == Side::Sell && dn.is_some_and(|d| px <= d + EPS && q.last <= d + EPS) {
                continue;
            }
            let mut cap = budget;
            let (fpx, extra_cap) = match o.kind {
                OrderKind::Market => (px, depth),
                OrderKind::Limit { price: l } => {
                    let marketable = match o.side {
                        Side::Buy => q.ask1.is_some_and(|a| a <= l + EPS),
                        Side::Sell => q.bid1.is_some_and(|b| b >= l - EPS),
                    };
                    let through = match o.side {
                        Side::Buy => q.last < l - EPS,
                        Side::Sell => q.last > l + EPS,
                    };
                    let touch = (q.last - l).abs() < EPS;
                    if marketable {
                        (px, depth)
                    } else if through {
                        (l, None)
                    } else if touch && self.cfg.limit_fill == LimitFill::TouchQueue {
                        let ahead = o.queue_ahead - dv;
                        self.orders.get_mut(&id).unwrap().queue_ahead = ahead.max(0.0);
                        if ahead >= 0.0 {
                            continue;
                        }
                        (l, Some(-ahead))
                    } else {
                        continue;
                    }
                }
            };
            if let Some(d) = extra_cap {
                cap = cap.min(d.floor() as i64);
            }
            let mut n = cap.min(rest);
            if lot > 1 && n < rest {
                n = n / lot * lot; // A-share partial fills keep whole lots
            }
            if n <= 0 {
                continue;
            }
            budget -= n;
            let f = self.apply_fill(id, fpx, n, q);
            fills.push(f);
        }
        fills
    }

    fn apply_fill(&mut self, id: u64, px: f64, n: i64, q: &Quote) -> Fill {
        let o = self.orders.get(&id).unwrap().clone();
        let market = self.inst[&o.symbol].setup.market.clone();
        let rest_before = o.qty - o.filled;
        let release = o.frozen * n as f64 / rest_before as f64;
        let fee;
        let mut realized = 0.0;
        let mut ct_lots = 0;
        let day = q.trading_day.clone();
        let p = self.account.positions.entry(o.symbol.clone()).or_default();
        let v = px * n as f64;
        let mut commission_paid = o.commission_paid;
        match &market {
            Market::AShare { .. } => {
                let c = &self.cfg.ashare_costs;
                let total = ((o.filled_value + v) * c.commission).max(c.min_commission);
                let comm = total - commission_paid;
                commission_paid = total;
                let (sb, ss) = scorer::stamp_duty_sides(&day);
                match o.side {
                    Side::Buy => {
                        fee = comm + v * sb;
                        p.long_cost = (p.long_cost * p.long() as f64 + v) / (p.long() + n) as f64;
                        p.long_today += n;
                        self.account.cash -= v + fee;
                    }
                    Side::Sell => {
                        fee = comm + v * ss;
                        realized = (px - p.long_cost) * n as f64;
                        p.long_yd -= n;
                        p.long_frozen -= n;
                        self.account.cash += v - fee;
                    }
                }
            }
            Market::Futures { spec } => {
                let (fo, fc, fct, per_lot) = fee_rates(&spec.fee);
                let rate = |r: f64, lots: i64| {
                    if per_lot {
                        r * lots as f64
                    } else {
                        r * px * spec.multiplier * lots as f64
                    }
                };
                if o.offset == Offset::Open {
                    fee = rate(fo, n);
                    match o.side {
                        Side::Buy => {
                            p.long_cost =
                                (p.long_cost * p.long() as f64 + v) / (p.long() + n) as f64;
                            p.long_today += n;
                            p.long_opened_today += n;
                        }
                        Side::Sell => {
                            p.short_cost =
                                (p.short_cost * p.short() as f64 + v) / (p.short() + n) as f64;
                            p.short_today += n;
                            p.short_opened_today += n;
                        }
                    }
                } else {
                    let long = o.side == Side::Sell;
                    let (today, yd, ct_charged, opened, cost) = if long {
                        (
                            &mut p.long_today,
                            &mut p.long_yd,
                            &mut p.long_ct_charged,
                            p.long_opened_today,
                            p.long_cost,
                        )
                    } else {
                        (
                            &mut p.short_today,
                            &mut p.short_yd,
                            &mut p.short_ct_charged,
                            p.short_opened_today,
                            p.short_cost,
                        )
                    };
                    // which bucket the lots come from, and which fee tier
                    let (from_today, from_yd) = match (o.offset, spec.close_order()) {
                        (Offset::CloseToday, _) => (n, 0),
                        (Offset::CloseYesterday, _) => (0, n),
                        // SHFE plain close: cheaper bucket first
                        (_, CloseOrder::TraderChooses) if fct <= fc => {
                            let t = n.min(*today);
                            (t, n - t)
                        }
                        // others: old lots first
                        _ => {
                            let y = n.min(*yd);
                            (n - y, y)
                        }
                    };
                    *today -= from_today;
                    *yd -= from_yd;
                    ct_lots = match spec.close_order() {
                        CloseOrder::TraderChooses | CloseOrder::Fifo => from_today,
                        CloseOrder::FifoFeeTodayFirst => {
                            // fee at the close-today rate first while lots opened today exist
                            let c = n.min((opened - *ct_charged).max(0));
                            *ct_charged += c;
                            c
                        }
                    };
                    fee = rate(fct, ct_lots) + rate(fc, n - ct_lots);
                    realized =
                        if long { px - cost } else { cost - px } * spec.multiplier * n as f64;
                    if long {
                        p.long_frozen -= n;
                    } else {
                        p.short_frozen -= n;
                    }
                }
                self.account.cash += realized - fee;
            }
        }
        self.account.frozen = (self.account.frozen - release).max(0.0);
        self.account.fees += fee;
        self.account.realized += realized;
        let o = self.orders.get_mut(&id).unwrap();
        o.filled += n;
        o.frozen -= release;
        o.commission_paid = commission_paid;
        o.filled_value += v;
        o.status = if o.filled == o.qty {
            OrderStatus::Filled
        } else {
            OrderStatus::PartFilled
        };
        if o.status == OrderStatus::Filled {
            self.account.frozen = (self.account.frozen - o.frozen).max(0.0);
            o.frozen = 0.0;
        }
        let (sym, side, offset) = (o.symbol.clone(), o.side, o.offset);
        self.recompute_margin();
        let f = Fill {
            order_id: id,
            symbol: sym,
            side,
            offset,
            price: px,
            qty: n,
            fee,
            close_today_lots: ct_lots,
            realized,
            ts: q.ts.clone(),
            trading_day: day,
        };
        self.fills.push(f.clone());
        self.push(Event::Filled { fill: f.clone() });
        f
    }

    fn recompute_margin(&mut self) {
        let mut m = 0.0;
        for (sym, p) in &self.account.positions {
            if let Some(Market::Futures { spec }) = self.inst.get(sym).map(|i| &i.setup.market) {
                m += (p.long_cost * p.long() as f64 + p.short_cost * p.short() as f64)
                    * spec.multiplier
                    * spec.margin_rate;
            }
        }
        self.account.margin = m;
    }

    /// End of trading day: expire working orders, mark futures to the settlement
    /// price (exchange settlement if set, else last), roll today → yesterday (T+1).
    pub fn settle(&mut self) {
        let Some(day) = self.account.trading_day.clone() else {
            return;
        };
        let ids: Vec<u64> = self
            .orders
            .values()
            .filter(|o| matches!(o.status, OrderStatus::Working | OrderStatus::PartFilled))
            .map(|o| o.id)
            .collect();
        for id in ids {
            self.finish(id, OrderStatus::Expired, "day order expired at settlement");
        }
        let mut prices = BTreeMap::new();
        let syms: Vec<String> = self.inst.keys().cloned().collect();
        for sym in syms {
            let px = {
                let i = &self.inst[&sym];
                i.settle.or(i.last.as_ref().map(|q| q.last))
            };
            let Some(px) = px else { continue };
            prices.insert(sym.clone(), px);
            let market = self.inst[&sym].setup.market.clone();
            if let Some(p) = self.account.positions.get_mut(&sym) {
                if let Market::Futures { spec } = &market {
                    let pnl = (px - p.long_cost) * spec.multiplier * p.long() as f64
                        + (p.short_cost - px) * spec.multiplier * p.short() as f64;
                    self.account.cash += pnl;
                    self.account.realized += pnl;
                    if p.long() > 0 {
                        p.long_cost = px;
                    }
                    if p.short() > 0 {
                        p.short_cost = px;
                    }
                }
                p.long_yd += p.long_today;
                p.long_today = 0;
                p.short_yd += p.short_today;
                p.short_today = 0;
                p.long_ct_charged = 0;
                p.short_ct_charged = 0;
                p.long_opened_today = 0;
                p.short_opened_today = 0;
            }
            let i = self.inst.get_mut(&sym).unwrap();
            i.setup.prev_close = px;
            i.settle = None;
            i.last_cum = None;
        }
        self.account
            .positions
            .retain(|_, p| p.long() != 0 || p.short() != 0);
        self.recompute_margin();
        let ev = Event::Settled {
            trading_day: day,
            prices,
            cash: self.account.cash,
            equity: self.equity(),
            margin: self.account.margin,
        };
        self.push(ev);
    }

    /// Signal → orders: market orders moving the net position toward `target`
    /// (closing first). A-share targets are rounded down to whole lots.
    pub fn rebalance(
        &mut self,
        s: &Strategy,
        symbol: &str,
        target: i64,
        ts: &str,
    ) -> Vec<Result<u64, Reject>> {
        let p = self
            .account
            .positions
            .get(symbol)
            .cloned()
            .unwrap_or_default();
        let ashare = matches!(
            self.inst.get(symbol).map(|i| &i.setup.market),
            Some(Market::AShare { .. })
        );
        let mut out = vec![];
        if ashare {
            let target = target.max(0) / 100 * 100;
            let cur = p.long();
            if target > cur {
                out.push(self.submit(
                    s,
                    symbol,
                    Side::Buy,
                    Offset::Open,
                    OrderKind::Market,
                    target - cur,
                    ts,
                ));
            } else if target < cur {
                let q = (cur - target).min(p.long_yd - p.long_frozen);
                out.push(self.submit(
                    s,
                    symbol,
                    Side::Sell,
                    Offset::Close,
                    OrderKind::Market,
                    q.max(1).min(cur - target),
                    ts,
                ));
            }
            return out;
        }
        let net = p.net();
        if target > net {
            let close = (p.short() - p.short_frozen).min(target - net).max(0);
            if close > 0 {
                out.push(self.submit(
                    s,
                    symbol,
                    Side::Buy,
                    Offset::Close,
                    OrderKind::Market,
                    close,
                    ts,
                ));
            }
            if target - net - close > 0 {
                out.push(self.submit(
                    s,
                    symbol,
                    Side::Buy,
                    Offset::Open,
                    OrderKind::Market,
                    target - net - close,
                    ts,
                ));
            }
        } else if target < net {
            let close = (p.long() - p.long_frozen).min(net - target).max(0);
            if close > 0 {
                out.push(self.submit(
                    s,
                    symbol,
                    Side::Sell,
                    Offset::Close,
                    OrderKind::Market,
                    close,
                    ts,
                ));
            }
            if net - target - close > 0 {
                out.push(self.submit(
                    s,
                    symbol,
                    Side::Sell,
                    Offset::Open,
                    OrderKind::Market,
                    net - target - close,
                    ts,
                ));
            }
        }
        out
    }

    /// QIFI-layout export of the paper account (simulated; not a broker statement).
    pub fn qifi(&self, account_cookie: &str) -> Value {
        let mut positions = serde_json::Map::new();
        for (sym, p) in &self.account.positions {
            let m = self.mark(sym).unwrap_or(0.0);
            positions.insert(sym.clone(), json!({
                "instrument_id": sym, "volume_long_today": p.long_today, "volume_long_his": p.long_yd,
                "volume_long": p.long(), "volume_short_today": p.short_today, "volume_short_his": p.short_yd,
                "volume_short": p.short(), "open_price_long": p.long_cost, "open_price_short": p.short_cost,
                "last_price": m,
            }));
        }
        let mut trades = serde_json::Map::new();
        let mut orders = serde_json::Map::new();
        for f in &self.fills {
            {
                trades.insert(format!("{}-{}", f.order_id, trades.len()), json!({
                    "order_id": f.order_id.to_string(), "instrument_id": f.symbol,
                    "direction": if f.side == Side::Buy {"BUY"} else {"SELL"},
                    "offset": match f.offset { Offset::Open => "OPEN", Offset::Close => "CLOSE", Offset::CloseToday => "CLOSETODAY", Offset::CloseYesterday => "CLOSEYESTERDAY" },
                    "price": f.price, "volume": f.qty, "commission": f.fee, "trade_date_time": f.ts,
                }));
            }
        }
        for o in self.orders.values() {
            orders.insert(o.id.to_string(), json!({
                "order_id": o.id.to_string(), "instrument_id": o.symbol, "volume_orign": o.qty,
                "volume_left": o.qty - o.filled, "status": serde_json::to_value(o.status).unwrap(),
                "price_type": match o.kind { OrderKind::Market => "ANY", OrderKind::Limit{..} => "LIMIT" },
                "limit_price": match o.kind { OrderKind::Market => Value::Null, OrderKind::Limit{price} => json!(price) },
            }));
        }
        let a = &self.account;
        json!({
            "account_cookie": account_cookie, "trading_day": a.trading_day,
            "accounts": { "pre_balance": a.initial_cash, "balance": self.equity(), "available": self.available(),
                "margin": a.margin, "frozen_margin": a.frozen, "commission": a.fees, "close_profit": a.realized,
                "position_profit": self.equity() - a.cash },
            "positions": positions, "trades": trades, "orders": orders,
            "cmsq": { "paper": true, "simulated_fills": true, "broker": Value::Null, "event_log_sha256": self.event_log_hash() },
        })
    }
}
