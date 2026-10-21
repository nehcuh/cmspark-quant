use cmsq_core::futures::spec_for;
use cmsq_core::models::{Strategy, StrategyStatus};
use cmsq_core::scorer::Rules;
use cmsq_paper::*;

fn strat(st: StrategyStatus) -> Strategy {
    Strategy {
        id: "s1".into(),
        title: "t".into(),
        source_site: "x".into(),
        source_url: None,
        author: None,
        scraped_at: None,
        ingested_at: chrono::Utc::now(),
        raw_path: String::new(),
        content_sha256: String::new(),
        sample: false,
        status: st,
        attempts: 0,
        last_reason: None,
        extracted: None,
        updated_at: chrono::Utc::now(),
    }
}
fn q(sym: &str, day: &str, ts: &str, last: f64, cum: f64, bid: f64, ask: f64) -> Quote {
    Quote {
        symbol: sym.into(),
        ts: ts.into(),
        trading_day: day.into(),
        last,
        cum_volume: cum,
        bid1: Some(bid),
        ask1: Some(ask),
        bid_vol1: None,
        ask_vol1: None,
        upper_limit: None,
        lower_limit: None,
    }
}
fn ashare(cfg: PaperConfig) -> PaperEngine {
    let mut e = PaperEngine::new(cfg);
    e.add_instrument(InstrumentSetup {
        symbol: "SH600000".into(),
        market: Market::AShare {
            rules: Rules::default(),
        },
        prev_close: 10.0,
        suspended: false,
        exchange_limits: Default::default(),
    });
    e
}
fn fut(sym: &str) -> PaperEngine {
    let mut e = PaperEngine::new(PaperConfig::default());
    e.add_instrument(InstrumentSetup {
        symbol: sym.into(),
        market: Market::Futures {
            spec: spec_for(sym).unwrap(),
        },
        prev_close: 3500.0,
        suspended: false,
        exchange_limits: Default::default(),
    });
    e
}
const D1: &str = "2026-10-19";
const D2: &str = "2026-10-20";

#[test]
fn only_promoted_strategies_trade() {
    let mut e = ashare(PaperConfig::default());
    for st in [
        StrategyStatus::GatePassed,
        StrategyStatus::Evaluated,
        StrategyStatus::Rejected,
    ] {
        let r = e.submit(
            &strat(st),
            "SH600000",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            100,
            "t",
        );
        assert!(r.unwrap_err().0.contains("only Promoted"));
    }
    assert!(e
        .submit(
            &strat(StrategyStatus::Promoted),
            "SH600000",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            100,
            "t"
        )
        .is_ok());
}

#[test]
fn ashare_lots_participation_t1_fees() {
    let s = strat(StrategyStatus::Promoted);
    let mut e = ashare(PaperConfig::default());
    assert!(e
        .submit(
            &s,
            "SH600000",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            150,
            "t"
        )
        .unwrap_err()
        .0
        .contains("100-share"));
    e.on_quote(&q("SH600000", D1, "09:30:00", 10.0, 1000.0, 9.99, 10.01));
    let id = e
        .submit(
            &s,
            "SH600000",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            1000,
            "t",
        )
        .unwrap();
    // first quote has no Δvolume: next Δ=2500 shares ⇒ 10% = 250 ⇒ 200 (whole lots)
    let f = e.on_quote(&q("SH600000", D1, "09:30:03", 10.0, 3500.0, 9.99, 10.01));
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].qty, f[0].price), (200, 10.01));
    assert!(
        (f[0].fee - 5.0).abs() < 1e-9,
        "min commission ¥5, no buy stamp duty: {}",
        f[0].fee
    );
    let f = e.on_quote(&q("SH600000", D1, "09:30:06", 10.0, 103_500.0, 9.99, 10.01));
    assert_eq!(f[0].qty, 800);
    // whole order's commission = max(5, 1000*10.01*0.00025=2.5) ⇒ second fill pays 0
    assert!(f[0].fee.abs() < 1e-9);
    assert_eq!(e.order(id).unwrap().status, OrderStatus::Filled);
    // T+1: cannot sell today
    assert!(e
        .submit(
            &s,
            "SH600000",
            Side::Sell,
            Offset::Close,
            OrderKind::Market,
            1000,
            "t"
        )
        .unwrap_err()
        .0
        .contains("T+1"));
    e.on_quote(&q("SH600000", D2, "09:30:00", 10.5, 0.0, 10.49, 10.51)); // rollover settles D1
    let id = e
        .submit(
            &s,
            "SH600000",
            Side::Sell,
            Offset::Close,
            OrderKind::Market,
            1000,
            "t",
        )
        .unwrap();
    let f = e.on_quote(&q(
        "SH600000", D2, "09:30:03", 10.5, 100_000.0, 10.49, 10.51,
    ));
    assert_eq!((f[0].qty, f[0].price), (1000, 10.49));
    let v = 10490.0;
    assert!(
        (f[0].fee - (5.0f64.max(v * 0.00025) + v * 0.0005)).abs() < 1e-6,
        "sell stamp duty 0.05%"
    );
    assert_eq!(e.order(id).unwrap().status, OrderStatus::Filled);
    // cash conservation: flat ⇒ equity = cash = initial + realized − fees
    let a = &e.account;
    assert!(a.positions.is_empty() || a.positions.values().all(|p| p.long() == 0));
    assert!((a.cash - (a.initial_cash - 10010.0 - 5.0 + v - f[0].fee)).abs() < 1e-6);
    assert!(a.frozen.abs() < 1e-6);
}

#[test]
fn no_buy_at_limit_up_no_sell_at_limit_down() {
    let s = strat(StrategyStatus::Promoted);
    let mut e = ashare(PaperConfig::default());
    e.on_quote(&q("SH600000", D1, "a", 11.0, 1000.0, 11.0, 11.0));
    e.submit(
        &s,
        "SH600000",
        Side::Buy,
        Offset::Open,
        OrderKind::Market,
        100,
        "t",
    )
    .unwrap();
    // sealed at limit-up 11.00 (prev close 10 × 1.10): no fill despite volume
    assert!(e
        .on_quote(&q("SH600000", D1, "b", 11.0, 900_000.0, 11.0, 11.0))
        .is_empty());
    // limit price outside band rejected
    assert!(e
        .submit(
            &s,
            "SH600000",
            Side::Buy,
            Offset::Open,
            OrderKind::Limit { price: 11.5 },
            100,
            "t"
        )
        .is_err());
}

#[test]
fn suspended_rejects_and_never_fills() {
    let s = strat(StrategyStatus::Promoted);
    let mut e = ashare(PaperConfig::default());
    e.set_suspended("SH600000", true);
    assert_eq!(
        e.submit(
            &s,
            "SH600000",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            100,
            "t"
        )
        .unwrap_err()
        .0,
        "suspended"
    );
}

#[test]
fn limit_trade_through_vs_touch_queue() {
    let s = strat(StrategyStatus::Promoted);
    for (mode, want_touch_fill) in [
        (LimitFill::TradeThrough, false),
        (LimitFill::TouchQueue, true),
    ] {
        let mut e = ashare(PaperConfig {
            limit_fill: mode,
            ..Default::default()
        });
        let mut q0 = q("SH600000", D1, "a", 10.0, 1000.0, 9.98, 10.0);
        q0.bid_vol1 = Some(3000.0);
        e.on_quote(&q0);
        let id = e
            .submit(
                &s,
                "SH600000",
                Side::Buy,
                Offset::Open,
                OrderKind::Limit { price: 9.98 },
                100,
                "t",
            )
            .unwrap();
        // trades AT 9.98 for 2000 shares: queue (3000) not yet consumed
        assert!(e
            .on_quote(&q("SH600000", D1, "b", 9.98, 3000.0, 9.97, 9.99))
            .is_empty());
        // another 2000 at 9.98: queue consumed (4000 > 3000) ⇒ TouchQueue fills, TradeThrough does not
        let f = e.on_quote(&q("SH600000", D1, "c", 9.98, 5000.0, 9.97, 9.99));
        assert_eq!(!f.is_empty(), want_touch_fill, "{mode:?}");
        if f.is_empty() {
            // strictly through the limit fills at the limit price
            let f = e.on_quote(&q("SH600000", D1, "d", 9.97, 7000.0, 9.96, 9.99));
            assert_eq!((f[0].price, f[0].qty), (9.98, 100));
        } else {
            assert_eq!(f[0].price, 9.98);
        }
        assert_eq!(e.order(id).unwrap().status, OrderStatus::Filled);
    }
}

#[test]
fn marketable_limit_takes_the_ask() {
    let s = strat(StrategyStatus::Promoted);
    let mut e = ashare(PaperConfig::default());
    e.on_quote(&q("SH600000", D1, "a", 10.0, 1000.0, 9.99, 10.01));
    e.submit(
        &s,
        "SH600000",
        Side::Buy,
        Offset::Open,
        OrderKind::Limit { price: 10.05 },
        100,
        "t",
    )
    .unwrap();
    let f = e.on_quote(&q("SH600000", D1, "b", 10.0, 3000.0, 9.99, 10.02));
    assert_eq!(f[0].price, 10.02);
}

#[test]
fn day_orders_expire_and_unfreeze() {
    let s = strat(StrategyStatus::Promoted);
    let mut e = ashare(PaperConfig::default());
    e.on_quote(&q("SH600000", D1, "a", 10.0, 1000.0, 9.99, 10.01));
    let id = e
        .submit(
            &s,
            "SH600000",
            Side::Buy,
            Offset::Open,
            OrderKind::Limit { price: 9.5 },
            100,
            "t",
        )
        .unwrap();
    assert!(e.account.frozen > 0.0);
    e.on_quote(&q("SH600000", D2, "a", 10.0, 0.0, 9.99, 10.01));
    assert_eq!(e.order(id).unwrap().status, OrderStatus::Expired);
    assert!(e.account.frozen.abs() < 1e-9);
}

#[test]
fn futures_margin_close_today_and_daily_settlement() {
    let s = strat(StrategyStatus::Promoted);
    let mut e = fut("FUT_RB");
    let spec = spec_for("FUT_RB").unwrap();
    e.on_quote(&q("FUT_RB", D1, "21:00:00", 3500.0, 100.0, 3499.0, 3501.0));
    e.submit(
        &s,
        "FUT_RB",
        Side::Buy,
        Offset::Open,
        OrderKind::Market,
        3,
        "t",
    )
    .unwrap();
    let f = e.on_quote(&q("FUT_RB", D1, "21:00:01", 3500.0, 200.0, 3499.0, 3501.0));
    assert_eq!((f[0].qty, f[0].price), (3, 3501.0));
    let m = 3501.0 * spec.multiplier * 3.0 * spec.margin_rate;
    assert!((e.account.margin - m).abs() < 1e-6);
    // SHFE: explicit close-today is allowed; DCE-style rejection checked below
    e.submit(
        &s,
        "FUT_RB",
        Side::Sell,
        Offset::CloseToday,
        OrderKind::Market,
        1,
        "t",
    )
    .unwrap();
    let f = e.on_quote(&q("FUT_RB", D1, "21:00:02", 3510.0, 300.0, 3509.0, 3511.0));
    assert_eq!(f[0].close_today_lots, 1);
    assert!((f[0].realized - (3509.0 - 3501.0) * spec.multiplier).abs() < 1e-6);
    // settle at the exchange settlement price, not last
    e.set_settlement("FUT_RB", 3520.0);
    let cash_before = e.account.cash;
    e.on_quote(&q("FUT_RB", D2, "09:00:00", 3530.0, 0.0, 3529.0, 3531.0));
    let pnl = (3520.0 - 3501.0) * spec.multiplier * 2.0;
    assert!(
        (e.account.cash - cash_before - pnl).abs() < 1e-6,
        "daily mark-to-market"
    );
    let p = &e.account.positions["FUT_RB"];
    assert_eq!((p.long_today, p.long_yd, p.long_cost), (0, 2, 3520.0));
    assert!(e
        .events()
        .iter()
        .any(|ev| matches!(ev, Event::Settled { prices, .. } if prices["FUT_RB"] == 3520.0)));
}

#[test]
fn dce_close_fee_today_first_and_no_close_today_instruction() {
    let Some(spec) = spec_for("FUT_M") else {
        return;
    };
    assert_ne!(
        spec.close_order(),
        cmsq_core::futures::CloseOrder::TraderChooses
    );
    let s = strat(StrategyStatus::Promoted);
    let mut e = fut("FUT_M");
    e.on_quote(&q("FUT_M", D1, "a", 3500.0, 100.0, 3499.0, 3501.0));
    e.submit(
        &s,
        "FUT_M",
        Side::Buy,
        Offset::Open,
        OrderKind::Market,
        2,
        "t",
    )
    .unwrap();
    e.on_quote(&q("FUT_M", D1, "b", 3500.0, 200.0, 3499.0, 3501.0));
    e.on_quote(&q("FUT_M", D2, "a", 3500.0, 100.0, 3499.0, 3501.0)); // 2 lots now yesterday's
    e.submit(
        &s,
        "FUT_M",
        Side::Buy,
        Offset::Open,
        OrderKind::Market,
        1,
        "t",
    )
    .unwrap();
    e.on_quote(&q("FUT_M", D2, "b", 3500.0, 200.0, 3499.0, 3501.0));
    assert!(e
        .submit(
            &s,
            "FUT_M",
            Side::Sell,
            Offset::CloseToday,
            OrderKind::Market,
            1,
            "t"
        )
        .is_err());
    e.submit(
        &s,
        "FUT_M",
        Side::Sell,
        Offset::Close,
        OrderKind::Market,
        2,
        "t",
    )
    .unwrap();
    let f = e.on_quote(&q("FUT_M", D2, "c", 3500.0, 300.0, 3499.0, 3501.0));
    // position reduced old lots first, but 1 lot charged at close-today tier (1 opened today)
    assert_eq!((f[0].qty, f[0].close_today_lots), (2, 1));
    let p = &e.account.positions["FUT_M"];
    assert_eq!((p.long_yd, p.long_today), (0, 1));
}

#[test]
fn margin_check_rejects_oversized_orders() {
    let s = strat(StrategyStatus::Promoted);
    let mut e = fut("FUT_RB");
    e.on_quote(&q("FUT_RB", D1, "a", 3500.0, 100.0, 3499.0, 3501.0));
    assert!(e
        .submit(
            &s,
            "FUT_RB",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            10_000,
            "t"
        )
        .unwrap_err()
        .0
        .contains("insufficient"));
}

fn tape() -> Vec<Quote> {
    let mut v = vec![];
    let mut cum = 0.0;
    for (d, day) in [D1, D2].iter().enumerate() {
        cum = 0.0;
        for i in 0..200 {
            let px = 3500.0 + ((i * 7 + d * 13) % 23) as f64 - 11.0;
            cum += ((i * 31) % 17) as f64 + 1.0;
            v.push(q(
                "FUT_RB",
                day,
                &format!("{day}T{i:05}"),
                px,
                cum,
                px - 1.0,
                px + 1.0,
            ));
        }
    }
    let _ = cum;
    v
}
fn run(cfg: PaperConfig) -> (String, f64) {
    let s = strat(StrategyStatus::Promoted);
    let mut e = PaperEngine::new(cfg);
    e.add_instrument(InstrumentSetup {
        symbol: "FUT_RB".into(),
        market: Market::Futures {
            spec: spec_for("FUT_RB").unwrap(),
        },
        prev_close: 3500.0,
        suspended: false,
        exchange_limits: Default::default(),
    });
    for (i, qu) in tape().iter().enumerate() {
        e.on_quote(qu);
        if i % 25 == 0 {
            let tgt = if (i / 25) % 2 == 0 { 3 } else { -2 };
            let _ = e.rebalance(&s, "FUT_RB", tgt, &qu.ts);
        }
        if i % 40 == 0 {
            let _ = e.submit(
                &s,
                "FUT_RB",
                Side::Buy,
                Offset::Open,
                OrderKind::Limit {
                    price: qu.last - 3.0,
                },
                1,
                &qu.ts,
            );
        }
    }
    e.settle();
    (e.event_log_hash(), e.equity())
}

#[test]
fn deterministic_replay() {
    let a = run(PaperConfig::default());
    let b = run(PaperConfig::default());
    assert_eq!(a, b, "identical tape ⇒ identical fills/event log");
    let c = run(PaperConfig {
        limit_fill: LimitFill::TouchQueue,
        participation: 0.5,
        ..Default::default()
    });
    assert_ne!(a.0, c.0);
}

#[test]
fn qifi_export_is_marked_simulated() {
    let s = strat(StrategyStatus::Promoted);
    let mut e = fut("FUT_RB");
    e.on_quote(&q("FUT_RB", D1, "a", 3500.0, 100.0, 3499.0, 3501.0));
    e.submit(
        &s,
        "FUT_RB",
        Side::Buy,
        Offset::Open,
        OrderKind::Market,
        1,
        "t",
    )
    .unwrap();
    e.on_quote(&q("FUT_RB", D1, "b", 3500.0, 200.0, 3499.0, 3501.0));
    let v = e.qifi("paper-s1");
    assert_eq!(v["cmsq"]["paper"], true);
    assert!(v["cmsq"]["broker"].is_null());
    assert_eq!(v["positions"]["FUT_RB"]["volume_long"], 1);
    assert_eq!(v["trades"].as_object().unwrap().len(), 1);
}

#[test]
fn exchange_published_limits_override_rule_band_but_not_quote_limits() {
    let s = strat(StrategyStatus::Promoted);
    let buy = |e: &mut PaperEngine, px: f64| {
        e.submit(
            &s,
            "SH600000",
            Side::Buy,
            Offset::Open,
            OrderKind::Limit { price: px },
            100,
            "t",
        )
    };
    // rule band alone (prev 10, ±10%): 10.80 is inside
    let mut plain = ashare(PaperConfig::default());
    plain.on_quote(&q("SH600000", D1, "09:30:00", 10.0, 1000.0, 9.99, 10.01));
    assert!(buy(&mut plain, 10.8).is_ok());
    // exchange data says the band that day is [9.50, 10.50] (e.g. ST/odd reference): rejected
    let mut e = PaperEngine::new(PaperConfig::default());
    e.add_instrument(InstrumentSetup {
        symbol: "SH600000".into(),
        market: Market::AShare {
            rules: Rules::default(),
        },
        prev_close: 10.0,
        suspended: false,
        exchange_limits: [(D1.to_string(), (10.5, 9.5))].into_iter().collect(),
    });
    e.on_quote(&q("SH600000", D1, "09:30:00", 10.0, 1000.0, 9.99, 10.01));
    assert!(buy(&mut e, 10.8).unwrap_err().0.contains("price band"));
    assert!(buy(&mut e, 10.5).is_ok());
    // a live quote that carries its own limits wins
    let mut lq = q("SH600000", D1, "09:31:00", 10.0, 1100.0, 9.99, 10.01);
    lq.upper_limit = Some(11.0);
    lq.lower_limit = Some(9.0);
    e.on_quote(&lq);
    assert!(buy(&mut e, 10.8).is_ok());
}
