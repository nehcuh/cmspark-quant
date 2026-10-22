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

// ---- issue #38: snapshot state must be bound to the keyed audit chain ----
fn traded<'a>(store: &'a Store, id: &str) -> (PaperSession<'a>, f64) {
    let s = strat(StrategyStatus::Promoted);
    let mut ps = PaperSession::start(store, id, &s, engine()).unwrap();
    ps.engine
        .on_quote(&q("2026-10-19", "09:00:00", 3500., 100.));
    ps.engine
        .submit(
            &s,
            "FUT_RB",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            1,
            "09:00:00",
        )
        .unwrap();
    ps.engine
        .on_quote(&q("2026-10-19", "09:00:01", 3500., 1000.));
    let c = ps.engine.account.cash;
    (ps, c)
}

#[test]
fn snapshot_cash_tamper_must_be_refused() {
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(&d.path().join("db"), AuditConfig::insecure_test()).unwrap();
    let (ps, expected) = traded(&store, "snap");
    ps.stop().unwrap();
    assert_eq!(
        PaperSession::resume(&store, "snap")
            .unwrap()
            .engine
            .account
            .cash,
        expected
    );
    store.raw_batch("DROP TRIGGER paper_snapshots_no_update;
        UPDATE paper_snapshots SET state_json=json_set(state_json,'$.account.cash',2000000.0)
        WHERE session_id='snap' AND seq=(SELECT MAX(seq) FROM paper_snapshots WHERE session_id='snap');").unwrap();
    assert!(verify_chain(&store, "snap").is_ok());
    assert!(store.verify_audit().unwrap().ok);
    let r = PaperSession::resume(&store, "snap");
    assert!(r.is_err(), "unauthenticated snapshot accepted");
}

#[test]
fn snapshot_transplanted_from_another_session_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(&d.path().join("db"), AuditConfig::insecure_test()).unwrap();
    let (a, _) = traded(&store, "a");
    a.stop().unwrap();
    let s = strat(StrategyStatus::Promoted);
    let b = PaperSession::start(&store, "b", &s, engine()).unwrap();
    b.stop().unwrap();
    // copy a's latest state over b's (same seq 0 is not required: replace b's latest row)
    store.raw_batch("DROP TRIGGER paper_snapshots_no_update;
        UPDATE paper_snapshots SET state_json=(SELECT state_json FROM paper_snapshots WHERE session_id='a' ORDER BY seq DESC LIMIT 1),
          seq=(SELECT MAX(seq) FROM paper_snapshots WHERE session_id='a')
        WHERE session_id='b';").unwrap();
    assert!(
        PaperSession::resume(&store, "b").is_err(),
        "cross-session snapshot accepted"
    );
}

#[test]
fn snapshot_without_audit_binding_is_refused_not_reauthenticated() {
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(&d.path().join("db"), AuditConfig::insecure_test()).unwrap();
    let (ps, _) = traded(&store, "legacy");
    ps.stop().unwrap();
    let state = store.latest_paper_snapshot("legacy").unwrap().unwrap();
    // a legacy/unknown snapshot row (no audit binding) at a newer seq
    store
        .insert_paper_snapshot("legacy", state.0 + 1, None, 0.0, &state.1)
        .unwrap();
    assert!(PaperSession::resume(&store, "legacy").is_err());
}

// ---- issue #39: stop must persist quote-only state ----
#[test]
fn stop_must_preserve_quote_only_state() {
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(&d.path().join("db"), AuditConfig::insecure_test()).unwrap();
    let (mut ps, _) = traded(&store, "mark");
    ps.flush().unwrap();
    ps.engine
        .on_quote(&q("2026-10-20", "09:00:00", 3500., 100.));
    ps.flush().unwrap();
    ps.engine
        .on_quote(&q("2026-10-20", "14:59:00", 3600., 2000.));
    ps.flush().unwrap();
    let expected = ps.engine.equity();
    let before = serde_json::to_value(&ps.engine).unwrap();
    ps.stop().unwrap();
    let r = PaperSession::resume(&store, "mark").unwrap();
    assert_eq!(r.engine.equity(), expected);
    let mut after = serde_json::to_value(&r.engine).unwrap();
    // only the checkpoint marker event may differ
    for k in ["event_seq", "event_chain"] {
        after[k] = before[k].clone();
    }
    assert_eq!(
        after, before,
        "resumed engine state differs from the stopped one"
    );
}

#[test]
fn resumed_session_replays_identically_to_uninterrupted_one() {
    let tape = |day: &str, ts: &str, px: f64, cum: f64| q(day, ts, px, cum);
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(&d.path().join("db"), AuditConfig::insecure_test()).unwrap();
    let s = strat(StrategyStatus::Promoted);
    // uninterrupted reference
    let mut refe = engine();
    refe.on_quote(&tape("2026-10-19", "09:00:00", 3500., 100.));
    refe.on_quote(&tape("2026-10-19", "09:00:05", 3510., 400.));
    refe.submit(
        &s,
        "FUT_RB",
        Side::Buy,
        Offset::Open,
        OrderKind::Market,
        3,
        "09:00:06",
    )
    .unwrap();
    let rf: Vec<_> = refe.on_quote(&tape("2026-10-19", "09:00:07", 3510., 430.));
    // stop after the quote-only update, resume, continue with the same tape
    let mut ps = PaperSession::start(&store, "rp", &s, engine()).unwrap();
    ps.engine
        .on_quote(&tape("2026-10-19", "09:00:00", 3500., 100.));
    ps.engine
        .on_quote(&tape("2026-10-19", "09:00:05", 3510., 400.));
    ps.stop().unwrap();
    let mut r = PaperSession::resume(&store, "rp").unwrap();
    r.engine
        .submit(
            &s,
            "FUT_RB",
            Side::Buy,
            Offset::Open,
            OrderKind::Market,
            3,
            "09:00:06",
        )
        .unwrap();
    let f: Vec<_> = r
        .engine
        .on_quote(&tape("2026-10-19", "09:00:07", 3510., 430.));
    assert_eq!(
        f, rf,
        "stale last_cum after resume would make volume reusable"
    );
    assert_eq!(r.engine.equity(), refe.equity());
}
