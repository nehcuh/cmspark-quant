//! Trade-level statistics from the trusted simulator's fills: FIFO round trips,
//! win rate, profit/loss ratio, average holding period.
//!
//! DISPLAY ONLY. Nothing here feeds the gate, the DSR or the trial ledger
//! (`gate.rs` never imports this module; a unit test greps for it). Win rate
//! and P/L ratio are easy to game and say nothing about significance.
use crate::scorer::Fill;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoundTrip {
    pub entry_date: String,
    pub exit_date: String,
    /// +1 long, -1 short
    pub side: i8,
    /// quantity on the adjusted scale (shares / lots)
    pub qty: f64,
    pub entry_price: f64,
    pub exit_price: f64,
    /// bars held (exit index − entry index)
    pub bars_held: usize,
    /// bar index of the exit fill
    pub exit_t: usize,
    /// P/L in yuan net of the entry and exit fill costs (roll legs excluded, see `roll_costs`)
    pub pnl: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TradeStats {
    pub display_only: bool,
    pub round_trips: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: Option<f64>,
    pub avg_win: Option<f64>,
    pub avg_loss: Option<f64>,
    /// average win / |average loss|
    pub pl_ratio: Option<f64>,
    /// gross wins / |gross losses|
    pub profit_factor: Option<f64>,
    pub avg_bars_held: Option<f64>,
    pub max_bars_held: usize,
    pub net_pnl: f64,
    /// costs of futures roll legs (not attributed to any round trip)
    pub roll_costs: f64,
    /// position still open after the last bar (not counted as a round trip)
    pub open_qty: f64,
    pub note: String,
}

/// FIFO-match `fills` into round trips. `mult` = contract multiplier (1 for shares).
/// Prices are matched on the adjusted scale so splits, dividends and contract
/// rolls do not create fake P/L.
pub fn round_trips(fills: &[Fill], mult: f64) -> (Vec<RoundTrip>, f64, f64) {
    // open queue: (t, date, signed qty remaining, adj price, cost per unit)
    let mut q: VecDeque<(usize, String, f64, f64, f64)> = VecDeque::new();
    let mut out = vec![];
    let mut roll = 0.0;
    for f in fills {
        if f.kind.starts_with("roll") {
            roll += f.cost;
            continue;
        }
        if f.qty.abs() < 1e-12 {
            continue;
        }
        let unit_cost = f.cost / f.qty.abs();
        let mut left = f.qty;
        while left.abs() > 1e-9 {
            match q.front_mut() {
                Some(o) if o.2.signum() != left.signum() => {
                    let take = o.2.abs().min(left.abs());
                    let side = o.2.signum();
                    let pnl = side * take * (f.adj_price - o.3) * mult - take * (o.4 + unit_cost);
                    out.push(RoundTrip {
                        entry_date: o.1.clone(),
                        exit_date: f.date.clone(),
                        side: side as i8,
                        qty: take,
                        entry_price: o.3,
                        exit_price: f.adj_price,
                        bars_held: f.t.saturating_sub(o.0),
                        exit_t: f.t,
                        pnl,
                    });
                    o.2 -= side * take;
                    left += side * take;
                    if o.2.abs() <= 1e-9 {
                        q.pop_front();
                    }
                }
                _ => {
                    q.push_back((f.t, f.date.clone(), left, f.adj_price, unit_cost));
                    left = 0.0;
                }
            }
        }
    }
    let open: f64 = q.iter().map(|o| o.2).sum();
    (out, roll, open)
}

/// Aggregate round trips. Several FIFO slices closed by the same exit fill and
/// opened by the same entry fill are one trade, so slices are merged per
/// (entry date, exit date, side) before counting.
pub fn stats(fills: &[Fill], mult: f64) -> TradeStats {
    stats_window(fills, mult, 0)
}

/// Stats for round trips whose EXIT falls at bar index ≥ `from_t`. Matching runs
/// over ALL fills, so a position opened before the window and closed inside it
/// is one trade (not a phantom opposite-side entry).
pub fn stats_window(fills: &[Fill], mult: f64, from_t: usize) -> TradeStats {
    let (rts, roll, open) = round_trips(fills, mult);
    let rts: Vec<RoundTrip> = rts.into_iter().filter(|r| r.exit_t >= from_t).collect();
    let roll = if from_t == 0 {
        roll
    } else {
        fills
            .iter()
            .filter(|f| f.kind.starts_with("roll") && f.t >= from_t)
            .map(|f| f.cost)
            .sum()
    };
    let mut merged: Vec<RoundTrip> = vec![];
    for r in rts {
        match merged.last_mut() {
            Some(m)
                if m.entry_date == r.entry_date
                    && m.exit_date == r.exit_date
                    && m.side == r.side =>
            {
                m.pnl += r.pnl;
                m.qty += r.qty;
            }
            _ => merged.push(r),
        }
    }
    let wins: Vec<f64> = merged
        .iter()
        .filter(|r| r.pnl > 0.0)
        .map(|r| r.pnl)
        .collect();
    let losses: Vec<f64> = merged
        .iter()
        .filter(|r| r.pnl <= 0.0)
        .map(|r| r.pnl)
        .collect();
    let n = merged.len();
    let mean = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    let (aw, al) = (mean(&wins), mean(&losses));
    let gl: f64 = losses.iter().sum();
    TradeStats {
        display_only: true,
        round_trips: n,
        wins: wins.len(),
        losses: losses.len(),
        win_rate: (n > 0).then(|| wins.len() as f64 / n as f64),
        avg_win: aw,
        avg_loss: al,
        pl_ratio: match (aw, al) { (Some(w), Some(l)) if l < 0.0 => Some(w / -l), _ => None },
        profit_factor: (gl < 0.0).then(|| wins.iter().sum::<f64>() / -gl),
        avg_bars_held: (n > 0).then(|| merged.iter().map(|r| r.bars_held as f64).sum::<f64>() / n as f64),
        max_bars_held: merged.iter().map(|r| r.bars_held).max().unwrap_or(0),
        net_pnl: merged.iter().map(|r| r.pnl).sum(),
        roll_costs: roll,
        open_qty: open,
        note: "display only — never used by the gate; FIFO round trips from host fills, P/L net of fill costs on the adjusted price scale, breakeven counted as a loss".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn f(t: usize, qty: f64, px: f64, cost: f64) -> Fill {
        Fill {
            t,
            date: format!("d{t:03}"),
            qty,
            price: px,
            adj_price: px,
            cost,
            kind: if qty > 0.0 {
                "buy".into()
            } else {
                "sell".into()
            },
        }
    }

    #[test]
    fn fifo_round_trips_and_ratios() {
        // win 100*(11-10)-2 = 98 held 3; loss 100*(9-10)-2 = -102 held 2; open 50 left
        let fl = vec![
            f(1, 100.0, 10.0, 1.0),
            f(4, -100.0, 11.0, 1.0),
            f(5, 150.0, 10.0, 1.5),
            f(7, -100.0, 9.0, 1.0),
        ];
        let s = stats(&fl, 1.0);
        assert_eq!((s.round_trips, s.wins, s.losses), (2, 1, 1));
        assert!(
            (s.avg_win.unwrap() - 98.0).abs() < 1e-9 && (s.avg_loss.unwrap() + 102.0).abs() < 1e-9,
            "{s:?}"
        );
        assert!((s.pl_ratio.unwrap() - 98.0 / 102.0).abs() < 1e-9);
        assert_eq!(s.win_rate, Some(0.5));
        assert_eq!(s.avg_bars_held, Some(2.5));
        assert!((s.open_qty - 50.0).abs() < 1e-9);
        assert!(s.display_only);
    }

    #[test]
    fn short_and_flip_with_multiplier() {
        // short 2 lots at 4000, buy 3 at 3900: closes short (+2*100*10 = 2000) and opens 1 long
        let fl = vec![
            f(1, -2.0, 4000.0, 0.0),
            f(3, 3.0, 3900.0, 0.0),
            f(5, -1.0, 3950.0, 0.0),
        ];
        let s = stats(&fl, 10.0);
        assert_eq!(s.round_trips, 2);
        assert!((s.net_pnl - (2000.0 + 500.0)).abs() < 1e-9, "{s:?}");
        assert_eq!(s.open_qty, 0.0);
    }

    #[test]
    fn empty_and_roll_costs() {
        let s = stats(&[], 1.0);
        assert_eq!((s.round_trips, s.win_rate, s.pl_ratio), (0, None, None));
        let mut r = f(2, -1.0, 100.0, 5.0);
        r.kind = "roll_close".into();
        let s = stats(
            &[
                f(1, 1.0, 100.0, 0.0),
                r.clone(),
                Fill { qty: 1.0, ..r },
                f(3, -1.0, 110.0, 0.0),
            ],
            1.0,
        );
        assert_eq!(s.round_trips, 1);
        assert!((s.roll_costs - 10.0).abs() < 1e-9 && (s.net_pnl - 10.0).abs() < 1e-9);
    }

    #[test]
    fn window_keeps_trade_opened_before_it() {
        let fl = vec![
            f(1, 100.0, 10.0, 0.0),
            f(4, -100.0, 11.0, 0.0),
            f(6, 100.0, 10.0, 0.0),
            f(9, -100.0, 12.0, 0.0),
        ];
        let s = stats_window(&fl, 1.0, 8);
        assert_eq!((s.round_trips, s.wins), (1, 1));
        assert!(
            (s.net_pnl - 200.0).abs() < 1e-9 && s.avg_bars_held == Some(3.0),
            "{s:?}"
        );
    }

    #[test]
    fn gate_never_uses_trade_stats() {
        let src = include_str!("gate.rs");
        assert!(
            !src.contains("tradestats") && !src.contains("TradeStats") && !src.contains("win_rate")
        );
    }
}
