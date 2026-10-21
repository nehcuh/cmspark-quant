//! Concurrency races on the persistence layer, on both backends:
//! audit chain linearity + anchor ordering, trial-ledger N, vault one-shot,
//! strategy CAS, and PostgreSQL least-privilege / append-only enforcement.
//! SQLite tests open one Store (= one connection) per thread on the same file,
//! which is what separate processes do. PostgreSQL tests (`#[ignore]`, run by
//! scripts/test_pg.sh) use a fresh schema per test on the deployed database.
use cmsq_core::audit::AuditConfig;
use cmsq_core::store::{NewTrial, Store};
use cmsq_core::{p, StrategyStatus};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

type Factory = Arc<dyn Fn() -> Store + Send + Sync>;

fn cfg(dir: &Path) -> AuditConfig {
    AuditConfig {
        signer: None,
        chain: String::new(),
        key: b"race-test-key-0123456789".to_vec(),
        anchor: Some(dir.join("anchors/race.anchor")),
    }
}

fn sqlite(dir: &Path) -> Factory {
    let (db, c) = (dir.join("race.db"), cfg(dir));
    Store::open(&db, c.clone()).unwrap();
    Arc::new(move || Store::open(&db, c.clone()).unwrap())
}

fn pg_env() -> (String, String) {
    let f = std::env::var("CMSQ_TEST_DB_ENV")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap()).join(".config/cmsq/db.env")
        });
    let t = std::fs::read_to_string(&f).unwrap_or_else(|_| {
        panic!(
            "PostgreSQL required: run scripts/db_up.sh ({} missing)",
            f.display()
        )
    });
    let get = |k: &str| {
        t.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")).map(String::from))
            .unwrap_or_else(|| panic!("{k} missing in db.env"))
    };
    (get("CMSQ_DATABASE_URL"), get("CMSQ_DATABASE_MIGRATE_URL"))
}

fn pg(dir: &Path) -> Factory {
    let (app, owner) = pg_env();
    let schema = format!("t_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let c = cfg(dir);
    Store::open_pg(&app, Some(&owner), Some(&schema), 4, c.clone()).unwrap();
    Arc::new(move || Store::open_pg(&app, None, Some(&schema), 4, c.clone()).unwrap())
}

fn on_threads<T: Send + 'static>(
    n: usize,
    mk: &Factory,
    f: impl Fn(usize, &Store) -> T + Send + Sync + 'static,
) -> Vec<T> {
    let f = Arc::new(f);
    let bar = Arc::new(Barrier::new(n));
    let hs: Vec<_> = (0..n)
        .map(|i| {
            let (mk, f, bar) = (mk.clone(), f.clone(), bar.clone());
            std::thread::spawn(move || {
                let s = mk();
                bar.wait();
                f(i, &s)
            })
        })
        .collect();
    hs.into_iter().map(|h| h.join().unwrap()).collect()
}

fn strategy(s: &Store, id: &str) {
    s.raw_exec("INSERT INTO strategies(id,title,source_site,ingested_at,raw_path,content_sha256,status,updated_at) VALUES(?1,'t','x','2026-01-01T00:00:00Z','/dev/null',?1,'Extracted','2026-01-01T00:00:00Z')",
        p![id]).unwrap();
}

fn audit_linear(mk: Factory, dir: &Path) {
    on_threads(8, &mk, |i, s| {
        for k in 0..25 {
            s.audit(
                "race",
                "append",
                Some(&format!("w{i}")),
                serde_json::json!({"k": k}),
                true,
            )
            .unwrap();
        }
    });
    let s = mk();
    let r = s.verify_audit().unwrap();
    assert!(r.ok, "{r:?}");
    assert_eq!(r.entries, 200);
    assert_eq!(r.anchored_seq, Some(200));
    // anchor lines are strictly ordered (appended under the flock)
    let lines: Vec<i64> = std::fs::read_to_string(dir.join("anchors/race.anchor"))
        .unwrap()
        .lines()
        .map(|l| l.split(' ').next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(lines, (1..=200).collect::<Vec<_>>());
}

fn ledger_monotone(mk: Factory) {
    let ns = on_threads(16, &mk, |i, s| {
        let pos = vec![0.5; 10];
        let h = format!("p{i}");
        s.record_trial(&NewTrial {
            strategy_id: "s",
            job_id: &format!("j{i}"),
            idea_key: "idea",
            params_hash: &h,
            symbol: "X",
            dataset_sha: "d",
            bars: 10,
            score_start: "2024-01-02",
            score_end: "2024-01-15",
            sharpe_daily: 0.01 * i as f64,
            positions: &pos,
            lookahead: false,
            run_kind: "sweep",
            group_id: Some("g"),
        })
        .unwrap()
        .len()
    });
    let mut ns = ns;
    ns.sort();
    assert_eq!(
        ns,
        (1..=16).collect::<Vec<_>>(),
        "every concurrent trial must see a distinct ledger size"
    );
    assert_eq!(mk().trial_count().unwrap(), 16);
}

fn vault_once(mk: Factory) {
    // different posts, same code: only one may spend the idea's vault
    let wins = on_threads(8, &mk, |i, s| {
        let sid = format!("strategy-{i}");
        s.claim_vault(
            &sid,
            &format!("job{i}"),
            "idea",
            "SH600519",
            "same-code-sha",
            |touches, _| {
                touches
                    .iter()
                    .find(|t| t.code_sha.as_deref() == Some("same-code-sha"))
                    .map(|t| format!("{} spent it", t.strategy_id))
            },
        )
        .unwrap()
        .is_ok()
    });
    assert_eq!(wins.iter().filter(|w| **w).count(), 1, "{wins:?}");
    // the same strategy racing itself
    let wins = on_threads(8, &mk, |i, s| {
        s.claim_vault(
            "solo",
            &format!("j{i}"),
            "idea2",
            "SZ000001",
            &format!("c{i}"),
            |_, _| None,
        )
        .unwrap()
        .is_ok()
    });
    assert_eq!(wins.iter().filter(|w| **w).count(), 1, "{wins:?}");
    let s = mk();
    s.finish_vault("solo", false).unwrap();
    assert!(
        s.finish_vault("solo", true).is_err(),
        "verdict is written once"
    );
    assert!(
        s.raw_exec(
            "UPDATE vault_touches SET passed=1 WHERE strategy_id='solo'",
            &[]
        )
        .is_err(),
        "finalized row is frozen"
    );
    assert!(s.raw_exec("DELETE FROM vault_touches", &[]).is_err());
}

fn cas_single_winner(mk: Factory) {
    strategy(&mk(), "cas-1");
    let wins = on_threads(8, &mk, |_, s| {
        s.cas_status(
            "cas-1",
            StrategyStatus::Extracted,
            StrategyStatus::Backtesting,
            None,
        )
        .unwrap()
    });
    assert_eq!(wins.iter().filter(|w| **w).count(), 1);
}

#[test]
fn sqlite_audit_concurrent_appends_stay_linear() {
    let d = tempfile::tempdir().unwrap();
    audit_linear(sqlite(d.path()), d.path());
}
#[test]
fn sqlite_trial_ledger_n_is_distinct_and_monotone() {
    let d = tempfile::tempdir().unwrap();
    ledger_monotone(sqlite(d.path()));
}
#[test]
fn sqlite_vault_cannot_be_double_spent() {
    let d = tempfile::tempdir().unwrap();
    vault_once(sqlite(d.path()));
}
#[test]
fn sqlite_status_cas_has_one_winner() {
    let d = tempfile::tempdir().unwrap();
    cas_single_winner(sqlite(d.path()));
}

#[test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
fn pg_audit_concurrent_appends_stay_linear() {
    let d = tempfile::tempdir().unwrap();
    audit_linear(pg(d.path()), d.path());
}
#[test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
fn pg_trial_ledger_n_is_distinct_and_monotone() {
    let d = tempfile::tempdir().unwrap();
    ledger_monotone(pg(d.path()));
}
#[test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
fn pg_vault_cannot_be_double_spent() {
    let d = tempfile::tempdir().unwrap();
    vault_once(pg(d.path()));
}
#[test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
fn pg_status_cas_has_one_winner() {
    let d = tempfile::tempdir().unwrap();
    cas_single_winner(pg(d.path()));
}

/// The runtime role cannot rewrite evidence even with raw SQL: triggers block
/// UPDATE/DELETE/TRUNCATE and, not being the owner, it cannot drop them.
#[test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
fn pg_app_role_cannot_rewrite_or_unprotect_evidence() {
    let d = tempfile::tempdir().unwrap();
    let s = pg(d.path())();
    s.audit("a", "x", None, serde_json::json!({}), true)
        .unwrap();
    for sql in [
        "UPDATE audit SET actor='evil'",
        "DELETE FROM audit",
        "TRUNCATE audit",
        "DROP TRIGGER audit_append_only ON audit",
        "ALTER TABLE audit DISABLE TRIGGER ALL",
        "DELETE FROM trials",
        "UPDATE signals SET observe_only=0",
        "DROP TABLE audit",
    ] {
        assert!(s.raw_batch(sql).is_err(), "app role could run: {sql}");
    }
    assert!(s.raw_batch("INSERT INTO signals(strategy_id,job_id,symbol,date,target_position,observe_only,created_at) VALUES('a','b','c','d',1,0,'t')").is_err());
    assert!(s.verify_audit().unwrap().ok);
    assert_eq!(s.backend(), "postgres");
}

/// Even the OWNER cannot UPDATE/DELETE audit rows without first dropping the
/// trigger — and if it does, the HMAC chain + anchor still catch it.
#[test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
fn pg_owner_tamper_is_detected_by_chain() {
    let d = tempfile::tempdir().unwrap();
    let (_, owner) = pg_env();
    let schema = format!("t_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let s = Store::open_pg(&owner, Some(&owner), Some(&schema), 2, cfg(d.path())).unwrap();
    for i in 0..4 {
        s.audit("a", "x", None, serde_json::json!({"i": i}), true)
            .unwrap();
    }
    assert!(s
        .raw_batch("UPDATE audit SET details='{}' WHERE seq=2")
        .is_err());
    s.raw_batch("ALTER TABLE audit DISABLE TRIGGER audit_append_only; UPDATE audit SET details='{}' WHERE seq=2").unwrap();
    let r = s.verify_audit().unwrap();
    assert!(!r.ok && r.first_bad_seq == Some(2), "{r:?}");
}

#[test]
#[ignore = "requires PostgreSQL: scripts/test_pg.sh"]
fn pg_bars_go_to_a_timescale_hypertable() {
    let d = tempfile::tempdir().unwrap();
    let s = pg(d.path())();
    let bars: Vec<cmsq_core::market::Bar> = (1..=20)
        .map(|i| cmsq_core::market::Bar {
            date: format!("2024-01-{i:02}"),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
            adj_factor: if i % 2 == 0 { Some(1.0) } else { None },
        })
        .collect();
    assert_eq!(s.insert_bars("SH600000", "sha", &bars).unwrap(), 20);
    assert_eq!(
        s.insert_bars("SH600000", "sha", &bars).unwrap(),
        0,
        "idempotent"
    );
    assert_eq!(s.bar_count("SH600000").unwrap(), 20);
    let ht = s.db.run(|e| e.query("SELECT count(*) FROM timescaledb_information.hypertables WHERE hypertable_name IN ('bars','ticks') AND hypertable_schema=current_schema()", &[])).unwrap();
    assert_eq!(ht[0].i(0), 2, "bars and ticks are hypertables");
}
