//! Optional QIFI-style JSON export of a PROMOTED strategy's observe-only paper
//! account (the QUANTAXIS account-snapshot layout: `accounts` / `positions` /
//! `trades` / `orders`). Field names follow the public QIFI layout so
//! QUANTAXIS-family tools can read it; the content is cmsq's own: a simulated
//! account from the trusted host scorer, NOT a broker statement. There is no
//! order path: `orders` is always empty and `cmsq.observe_only` is true.
//!
//! Written from scratch from the field layout only; no QUANTAXIS / qa-rs code is used.
use crate::models::Signal;
use crate::scorer::{Fill, SimResult};
use serde_json::{json, Map, Value};

/// The part of a vault run the export needs; stored in the vault job's result.json.
pub fn account_inputs(
    symbol: &str,
    spec: Option<&crate::futures::ContractSpec>,
    sim: &SimResult,
    bars: &[crate::market::Bar],
    from_t: usize,
) -> Value {
    let n = bars.len();
    let last = n.saturating_sub(1);
    let start_equity = if from_t > 0 {
        sim.equity[from_t - 1]
    } else {
        crate::scorer::INITIAL_CAPITAL
    };
    let fills: Vec<&Fill> = sim.fills.iter().filter(|f| f.t >= from_t).collect();
    let mult = spec.map(|s| s.multiplier).unwrap_or(1.0);
    let margin = spec
        .map(|s| sim.final_units.abs() * sim.final_price * mult * s.margin_rate)
        .unwrap_or(0.0);
    json!({"symbol": symbol, "trading_day": bars.get(last).map(|b| b.date.clone()), "window_start": bars.get(from_t).map(|b| b.date.clone()),
        "start_equity": start_equity, "balance": sim.equity.get(last), "final_units": sim.final_units, "final_price": sim.final_price,
        "multiplier": mult, "margin": margin, "exchange": spec.map(|s| s.exchange.clone()), "fills": fills,
        "commission": fills.iter().map(|f| f.cost).sum::<f64>(), "close_today_fees": sim.close_today_fees})
}

fn qifi_code(symbol: &str, exchange: Option<&str>) -> (String, String) {
    match (crate::futures::product_of(symbol), exchange) {
        (Some(p), Some(e)) => (e.to_string(), p),
        _ => {
            let ex = if symbol.starts_with("SH") {
                "SSE"
            } else if symbol.starts_with("SZ") {
                "SZSE"
            } else if symbol.starts_with("BJ") {
                "BSE"
            } else {
                ""
            };
            (
                ex.to_string(),
                symbol
                    .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                    .to_string(),
            )
        }
    }
}

/// Build the QIFI-style document. `inputs` = [`account_inputs`] from the vault run.
pub fn build(
    strategy_id: &str,
    title: &str,
    inputs: &Value,
    signals: &[Signal],
    updated: &str,
) -> Value {
    let f = |k: &str| inputs[k].as_f64().unwrap_or(0.0);
    let symbol = inputs["symbol"].as_str().unwrap_or("");
    let (exchange, inst) = qifi_code(symbol, inputs["exchange"].as_str());
    let futures = inputs["exchange"].is_string();
    let (bal, start, units, px, mult, margin) = (
        f("balance"),
        f("start_equity"),
        f("final_units"),
        f("final_price"),
        f("multiplier"),
        f("margin"),
    );
    let market_value = units * px * mult;
    let available = if futures {
        bal - margin
    } else {
        bal - market_value.max(0.0)
    };
    // fills -> QIFI trades
    let mut trades = Map::new();
    if let Some(fl) = inputs["fills"].as_array() {
        for (i, x) in fl.iter().enumerate() {
            let q = x["qty"].as_f64().unwrap_or(0.0);
            let kind = x["kind"].as_str().unwrap_or("");
            let offset = match kind {
                "open" | "buy" => "OPEN",
                "close_today" => "CLOSETODAY",
                "roll_open" => "OPEN",
                _ => "CLOSE",
            };
            let id = format!(
                "cmsq-{}-{:05}",
                &strategy_id[..strategy_id.len().min(8)],
                i + 1
            );
            trades.insert(id.clone(), json!({"seqno": i + 1, "user_id": strategy_id, "trade_id": id, "exchange_id": exchange, "instrument_id": inst,
                "order_id": "", "exchange_trade_id": "", "direction": if q > 0.0 { "BUY" } else { "SELL" }, "offset": offset,
                "volume": q.abs(), "price": x["price"], "trade_date_time": x["date"], "commission": x["cost"], "cmsq_kind": kind}));
        }
    }
    let mut positions = Map::new();
    if units.abs() > 1e-9 {
        let (long, short) = if units > 0.0 {
            (units, 0.0)
        } else {
            (0.0, -units)
        };
        positions.insert(format!("{exchange}.{inst}"), json!({"user_id": strategy_id, "exchange_id": exchange, "instrument_id": inst,
            "volume_long_today": 0, "volume_long_his": long, "volume_long": long, "volume_short_today": 0, "volume_short_his": short, "volume_short": short,
            "last_price": px, "volume_multiple": mult, "margin_long": if units > 0.0 { margin } else { 0.0 }, "margin_short": if units < 0.0 { margin } else { 0.0 },
            "margin": margin, "market_value": market_value}));
    }
    json!({
        "databaseip": "", "account_cookie": strategy_id, "password": "", "portfolio": "cmsq-observe-only", "broker_name": "none (observe-only paper account)",
        "capital_password": "", "bank_password": "", "bankid": "", "investor_name": title, "money": bal, "pub_host": "", "settlement": {},
        "taskid": "", "trade_host": "", "updatetime": updated, "wsuri": "", "bankname": "", "trading_day": inputs["trading_day"], "status": 200,
        "accounts": {"user_id": strategy_id, "currency": "CNY", "pre_balance": start, "deposit": 0.0, "withdraw": 0.0, "WithdrawQuota": available.max(0.0),
            "close_profit": bal - start, "commission": f("commission"), "premium": 0.0, "static_balance": start, "position_profit": 0.0, "float_profit": 0.0,
            "balance": bal, "margin": margin, "frozen_margin": 0.0, "frozen_commission": 0.0, "frozen_premium": 0.0, "available": available,
            "risk_ratio": if bal > 0.0 { margin / bal } else { 0.0 }},
        "positions": positions, "trades": trades, "orders": {}, "frozen": {}, "transfers": {}, "banks": {}, "event": {},
        "cmsq": {"observe_only": true, "source": "cmsq-core host scorer (trusted), vault holdout window", "window_start": inputs["window_start"],
            "close_today_fees": inputs["close_today_fees"], "note": "simulated paper account; not a broker statement; no orders were ever sent",
            "signals": signals.iter().map(|s| json!({"date": s.date, "symbol": s.symbol, "target_position": s.target_position, "job_id": s.job_id, "created_at": s.created_at})).collect::<Vec<_>>()},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::Bar;

    #[test]
    fn futures_short_account_snapshot() {
        let spec = crate::futures::spec_for("FUT_RB").unwrap();
        let bars: Vec<Bar> = (0..4)
            .map(|i| Bar {
                date: format!("2024-01-0{}", i + 2),
                open: 4000.0,
                high: 4000.0,
                low: 3900.0,
                close: 4000.0 - 10.0 * i as f64,
                volume: 1e5,
                adj_factor: Some(1.0),
            })
            .collect();
        let sim = crate::futures::simulate(
            &spec,
            &bars,
            &[-1.0; 4],
            &Default::default(),
            &Default::default(),
        );
        let inp = account_inputs("FUT_RB", Some(&spec), &sim, &bars, 1);
        let sig = Signal {
            strategy_id: "abcdef123".into(),
            job_id: "j".into(),
            symbol: "FUT_RB".into(),
            date: "2024-01-05".into(),
            target_position: -1.0,
            created_at: chrono::Utc::now(),
        };
        let q = build("abcdef123", "t", &inp, &[sig], "2024-01-05 15:00:00");
        let p = &q["positions"]["SHFE.RB"];
        assert!(
            p["volume_short"].as_f64().unwrap() > 0.0 && p["volume_long"] == 0.0,
            "{q:#}"
        );
        assert_eq!(q["orders"], json!({}));
        assert_eq!(q["cmsq"]["observe_only"], true);
        assert!(!q["trades"].as_object().unwrap().is_empty());
        let a = &q["accounts"];
        assert!(
            (a["balance"].as_f64().unwrap()
                - a["available"].as_f64().unwrap()
                - a["margin"].as_f64().unwrap())
            .abs()
                < 1e-6
        );
        assert_eq!(
            q["trades"].as_object().unwrap().values().next().unwrap()["offset"],
            "OPEN"
        );
    }

    #[test]
    fn ashare_flat_account() {
        let inp = json!({"symbol": "SH600519", "balance": 1.1e6, "start_equity": 1e6, "final_units": 0.0, "final_price": 10.0, "multiplier": 1.0, "margin": 0.0, "fills": [], "trading_day": "2024-01-05"});
        let q = build("s", "t", &inp, &[], "x");
        assert_eq!(q["positions"], json!({}));
        assert_eq!(q["accounts"]["available"], 1.1e6);
        assert_eq!(
            q["accounts"]["close_profit"].as_f64().unwrap().round(),
            100000.0
        );
    }
}
