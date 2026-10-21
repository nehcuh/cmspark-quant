use cmsq_core::audit::AuditConfig;
use cmsq_core::futures::spec_for;
use cmsq_core::models::{Strategy, StrategyStatus};
use cmsq_core::Store;
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
fn q(day: &str, ts: &str, last: f64, cum: f64) -> Quote {
    Quote {
        symbol: "FUT_RB".into(),
        ts: ts.into(),
        trading_day: day.into(),
        last,
        cum_volume: cum,
        bid1: Some(last - 1.0),
        ask1: Some(last + 1.0),
        bid_vol1: None,
        ask_vol1: Some(5.0),
        upper_limit: None,
        lower_limit: None,
    }
}
fn engine() -> PaperEngine {
    let mut e = PaperEngine::new(PaperConfig::default());
    e.add_instrument(InstrumentSetup {
        symbol: "FUT_RB".into(),
        market: Market::Futures {
            spec: spec_for("FUT_RB").unwrap(),
        },
        prev_close: 3500.0,
        suspended: false,
        exchange_limits: Default::default(),
    });
    e
}

#[test]
fn persist_resume_verify_and_tamper_detection() {
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(
        &d.path().join("db.sqlite"),
        AuditConfig {
            signer: None,
            chain: String::new(),
            key: b"k".to_vec(),
            anchor: None,
        },
    )
    .unwrap();
    lifecycle(&store);
    // an attacker who bypasses the trigger is caught by the chain
    store
        .raw_batch("DROP TRIGGER paper_events_no_update")
        .unwrap();
    store.raw_batch("UPDATE paper_events SET event_json = replace(event_json, '\"qty\":2', '\"qty\":20') WHERE kind='accepted'").unwrap();
    assert!(verify_chain(&store, "p1")
        .unwrap_err()
        .to_string()
        .contains("altered"));
    assert!(PaperSession::resume(&store, "p1").is_err());
}

/// PostgreSQL: same lifecycle through the least-privilege app role (scripts/test_pg.sh).
#[test]
#[ignore]
fn pg_paper_lifecycle() {
    let f = std::env::var("CMSQ_TEST_DB_ENV").expect("CMSQ_TEST_DB_ENV");
    let t = std::fs::read_to_string(f).unwrap();
    let get = |k: &str| {
        t.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")).map(String::from))
            .unwrap()
    };
    let schema = format!(
        "t_paper_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000_000_000
    );
    let c = AuditConfig {
        signer: None,
        chain: String::new(),
        key: b"k".to_vec(),
        anchor: None,
    };
    Store::open_pg(
        &get("CMSQ_DATABASE_URL"),
        Some(&get("CMSQ_DATABASE_MIGRATE_URL")),
        Some(&schema),
        2,
        c.clone(),
    )
    .unwrap();
    let store = Store::open_pg(&get("CMSQ_DATABASE_URL"), None, Some(&schema), 2, c).unwrap();
    lifecycle(&store);
    assert!(
        store.raw_batch("DELETE FROM paper_snapshots").is_err(),
        "append-only on PG"
    );
}

fn lifecycle(store: &Store) {
    assert!(PaperSession::start(store, "x", &strat(StrategyStatus::GatePassed), engine()).is_err());
    let s = strat(StrategyStatus::Promoted);
    let mut ps = PaperSession::start(store, "p1", &s, engine()).unwrap();
    ps.engine.on_quote(&q("2026-10-19", "a", 3500.0, 10.0));
    // a limit far from the market keeps a MAX queue estimate: must round-trip through JSON
    ps.engine
        .submit(
            &s,
            "FUT_RB",
            Side::Buy,
            Offset::Open,
            OrderKind::Limit { price: 3400.0 },
            1,
            "a",
        )
        .unwrap();
    ps.engine
        .submit(
            &s,
            "FUT_RB",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            2,
            "a",
        )
        .unwrap();
    ps.engine.on_quote(&q("2026-10-19", "b", 3500.0, 40.0));
    ps.flush().unwrap();
    ps.engine.set_settlement("FUT_RB", 3510.0);
    ps.engine.on_quote(&q("2026-10-20", "a", 3505.0, 10.0)); // rollover ⇒ Settled ⇒ snapshot on flush
    ps.flush().unwrap();
    let head = ps.engine.event_log_hash();
    let eq = ps.engine.equity();
    ps.stop().unwrap();

    let (n, h) = verify_chain(store, "p1").unwrap();
    assert_eq!(h, head);
    assert!(n >= 4);
    let r = PaperSession::resume(store, "p1").unwrap();
    assert_eq!(r.engine.event_log_hash(), head);
    assert!((r.engine.equity() - eq).abs() < 1e-9);
    assert_eq!(r.engine.account.positions["FUT_RB"].long_yd, 2);
    assert!(store.paper_equity_curve("p1").unwrap().len() >= 2);
    assert!(store.verify_audit().unwrap().ok);
    let acts: Vec<String> = store
        .audit_entries(None, 100)
        .unwrap()
        .into_iter()
        .map(|a| a.action)
        .collect();
    assert!(
        acts.iter().any(|a| a == "paper_events") && acts.iter().any(|a| a == "paper_session_stop")
    );

    // append-only: UPDATE blocked by trigger
    assert!(store
        .raw_batch("UPDATE paper_events SET event_json='{}' WHERE seq=2")
        .is_err());
}

#[test]
fn signal_driver_trades_next_day_and_replays_identically() {
    let run = || {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(
            &d.path().join("db.sqlite"),
            AuditConfig {
                signer: None,
                chain: String::new(),
                key: b"k".to_vec(),
                anchor: None,
            },
        )
        .unwrap();
        let s = strat(StrategyStatus::Promoted);
        let mut ps = PaperSession::start(&store, "p", &s, engine()).unwrap();
        let mut drv = SignalDriver::new(
            vec![("2026-10-19".into(), 0.5), ("2026-10-20".into(), -0.3)],
            "FUT_RB",
        );
        for (day, base) in [
            ("2026-10-19", 3500.0),
            ("2026-10-20", 3520.0),
            ("2026-10-21", 3490.0),
        ] {
            for i in 0..50 {
                drv.on_quote(
                    &mut ps,
                    &s,
                    &q(
                        day,
                        &format!("{day}T{i:03}"),
                        base + (i % 5) as f64,
                        100.0 * (i + 1) as f64,
                    ),
                )
                .unwrap();
            }
        }
        // signal dated 19th acts on the 20th (not on the 19th itself)
        assert_eq!(drv.rebalances[0].0, "2026-10-20");
        assert!(drv.rebalances[0].2 > 0 && drv.rebalances[1].2 < 0);
        assert!(ps.engine.account.positions["FUT_RB"].net() < 0);
        let h = ps.engine.event_log_hash();
        ps.stop().unwrap();
        assert_eq!(verify_chain(&store, "p").unwrap().1, h);
        h
    };
    assert_eq!(run(), run());
}
