//! Persistence. Primary store: an independently deployed PostgreSQL (TimescaleDB
//! image, `deploy/docker-compose.yml`) through a sqlx connection pool. SQLite is
//! kept only for unit tests and explicit offline mode (`CMSQ_DB=sqlite`).
//!
//! Concurrency contract (holds on both backends):
//! * audit append: chain head read + insert under the `Audit` lock, and the
//!   external anchor is appended while an exclusive flock on the anchor file is
//!   held, so seq/prev_hash stay linear and anchor lines stay ordered;
//! * trial ledger: [`Store::record_trial`] inserts and snapshots the ledger in one
//!   locked transaction, so every trial sees a distinct, monotone N;
//! * vault: [`Store::claim_vault`] runs the idea/code/correlation dedup check and
//!   the insert in one locked transaction (one-shot cannot be double-spent);
//! * strategy state: compare-and-swap ([`Store::cas_status`]).
pub mod sql;

use crate::audit::{self, AuditConfig, VerifyReport, GENESIS};
use crate::models::*;
use crate::p;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sql::{Backend, Lock, Pg, R, V};
use std::path::Path;

pub const SQLITE_SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS strategies(
  id TEXT PRIMARY KEY, title TEXT NOT NULL, source_site TEXT NOT NULL, source_url TEXT,
  author TEXT, scraped_at TEXT, ingested_at TEXT NOT NULL, raw_path TEXT NOT NULL,
  content_sha256 TEXT NOT NULL UNIQUE, sample INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, last_reason TEXT,
  extracted_json TEXT, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS jobs(
  id TEXT PRIMARY KEY, strategy_id TEXT NOT NULL REFERENCES strategies(id), backend TEXT NOT NULL,
  image TEXT NOT NULL, symbol TEXT NOT NULL, data_source TEXT NOT NULL, status TEXT NOT NULL,
  exit_code INTEGER, started_at TEXT NOT NULL, finished_at TEXT, duration_ms INTEGER,
  job_dir TEXT NOT NULL, error TEXT, result_json TEXT,
  owner_pid INTEGER, runner_identity TEXT, kind TEXT NOT NULL DEFAULT 'dev');
CREATE TABLE IF NOT EXISTS trials(
  id INTEGER PRIMARY KEY AUTOINCREMENT, strategy_id TEXT NOT NULL, job_id TEXT NOT NULL,
  idea_key TEXT NOT NULL, params_hash TEXT NOT NULL, symbol TEXT NOT NULL, dataset_sha TEXT NOT NULL,
  bars INTEGER NOT NULL, sharpe_daily REAL NOT NULL, positions_json TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS vault_touches(
  strategy_id TEXT PRIMARY KEY, job_id TEXT NOT NULL, passed INTEGER NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS evaluations(
  id TEXT PRIMARY KEY, strategy_id TEXT NOT NULL REFERENCES strategies(id), job_id TEXT NOT NULL,
  passed INTEGER NOT NULL, reasons_json TEXT NOT NULL, checks_json TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS signals(
  id INTEGER PRIMARY KEY AUTOINCREMENT, strategy_id TEXT NOT NULL, job_id TEXT NOT NULL,
  symbol TEXT NOT NULL, date TEXT NOT NULL, target_position REAL NOT NULL,
  observe_only INTEGER NOT NULL DEFAULT 1 CHECK(observe_only = 1), created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS audit(
  seq INTEGER PRIMARY KEY, ts TEXT NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
  target TEXT, details TEXT NOT NULL, success INTEGER NOT NULL, prev_hash TEXT NOT NULL, hash TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS research_runs(
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, strategy_id TEXT, spec_json TEXT NOT NULL,
  result_json TEXT, status TEXT NOT NULL, created_at TEXT NOT NULL, finished_at TEXT);
CREATE TABLE IF NOT EXISTS bars(
  symbol TEXT NOT NULL, date TEXT NOT NULL, snapshot_sha TEXT NOT NULL,
  open REAL NOT NULL, high REAL NOT NULL, low REAL NOT NULL, close REAL NOT NULL, volume REAL NOT NULL, adj_factor REAL,
  PRIMARY KEY(symbol, snapshot_sha, date));
CREATE TABLE IF NOT EXISTS ticks(
  instrument TEXT NOT NULL, ts TEXT NOT NULL, trading_day TEXT NOT NULL, last REAL NOT NULL, volume REAL NOT NULL,
  open_interest REAL, bid1 REAL, ask1 REAL, source TEXT NOT NULL);
CREATE TRIGGER IF NOT EXISTS audit_no_update BEFORE UPDATE ON audit
  BEGIN SELECT RAISE(ABORT, 'audit log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_no_delete BEFORE DELETE ON audit
  BEGIN SELECT RAISE(ABORT, 'audit log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS trials_no_update BEFORE UPDATE ON trials
  BEGIN SELECT RAISE(ABORT, 'trials is append-only'); END;
CREATE TRIGGER IF NOT EXISTS trials_no_delete BEFORE DELETE ON trials
  BEGIN SELECT RAISE(ABORT, 'trials is append-only'); END;
CREATE TRIGGER IF NOT EXISTS signals_no_update BEFORE UPDATE ON signals
  BEGIN SELECT RAISE(ABORT, 'signals is append-only'); END;
CREATE TRIGGER IF NOT EXISTS signals_no_delete BEFORE DELETE ON signals
  BEGIN SELECT RAISE(ABORT, 'signals is append-only'); END;
CREATE TRIGGER IF NOT EXISTS vault_no_delete BEFORE DELETE ON vault_touches
  BEGIN SELECT RAISE(ABORT, 'vault_touches is append-only'); END;
CREATE TABLE IF NOT EXISTS paper_sessions(
  id TEXT PRIMARY KEY, strategy_id TEXT NOT NULL, config_json TEXT NOT NULL, status TEXT NOT NULL,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS paper_events(
  session_id TEXT NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, event_json TEXT NOT NULL,
  chain TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(session_id, seq));
CREATE TABLE IF NOT EXISTS paper_snapshots(
  session_id TEXT NOT NULL, seq INTEGER NOT NULL, trading_day TEXT, equity REAL NOT NULL,
  state_json TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(session_id, seq));
CREATE TRIGGER IF NOT EXISTS paper_events_no_update BEFORE UPDATE ON paper_events
  BEGIN SELECT RAISE(ABORT, 'paper_events is append-only'); END;
CREATE TRIGGER IF NOT EXISTS paper_events_no_delete BEFORE DELETE ON paper_events
  BEGIN SELECT RAISE(ABORT, 'paper_events is append-only'); END;
CREATE TRIGGER IF NOT EXISTS paper_snapshots_no_update BEFORE UPDATE ON paper_snapshots
  BEGIN SELECT RAISE(ABORT, 'paper_snapshots is append-only'); END;
CREATE TRIGGER IF NOT EXISTS paper_snapshots_no_delete BEFORE DELETE ON paper_snapshots
  BEGIN SELECT RAISE(ABORT, 'paper_snapshots is append-only'); END;
"#;

/// Columns added after the first SQLite release (forward migration).
const SQLITE_COLUMNS: &[(&str, &str, &str)] = &[
    ("jobs", "owner_pid", "INTEGER"),
    ("jobs", "runner_identity", "TEXT"),
    ("jobs", "kind", "TEXT NOT NULL DEFAULT 'dev'"),
    ("jobs", "owner_host", "TEXT"),
    ("vault_touches", "idea_key", "TEXT"),
    ("vault_touches", "symbol", "TEXT"),
    ("vault_touches", "code_sha", "TEXT"),
    ("vault_touches", "finished", "INTEGER NOT NULL DEFAULT 0"),
    ("trials", "lookahead", "INTEGER NOT NULL DEFAULT 0"),
    ("trials", "run_kind", "TEXT NOT NULL DEFAULT 'single'"),
    ("trials", "group_id", "TEXT"),
    ("trials", "score_start", "TEXT NOT NULL DEFAULT ''"),
    ("trials", "score_end", "TEXT NOT NULL DEFAULT ''"),
    ("strategies", "run_owner", "TEXT"),
];
const SQLITE_LATE_TRIGGERS: &str = r#"
CREATE TRIGGER IF NOT EXISTS vault_finalized BEFORE UPDATE ON vault_touches WHEN OLD.finished <> 0
  BEGIN SELECT RAISE(ABORT, 'vault touch already finalized'); END;
CREATE TRIGGER IF NOT EXISTS vault_identity BEFORE UPDATE ON vault_touches
  WHEN NEW.strategy_id IS NOT OLD.strategy_id OR NEW.job_id IS NOT OLD.job_id OR NEW.code_sha IS NOT OLD.code_sha OR NEW.idea_key IS NOT OLD.idea_key
  BEGIN SELECT RAISE(ABORT, 'vault touch identity is immutable'); END;
"#;

static PG_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/pg");

/// Database connection settings. Resolved from env, then `<secrets>/db.env`
/// (written by `scripts/db_up.sh`, mode 0600, never committed).
#[derive(Clone, Default)]
pub struct DbConfig {
    /// runtime (least-privilege) URL
    pub url: Option<String>,
    /// owner URL used only to run migrations
    pub migrate_url: Option<String>,
    pub schema: Option<String>,
    pub max_conns: u32,
    /// `CMSQ_DB=sqlite`: explicit offline mode
    pub offline: bool,
}

impl std::fmt::Debug for DbConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbConfig")
            .field("url", &self.url.as_ref().map(|u| redact_url(u)))
            .field("schema", &self.schema)
            .field("offline", &self.offline)
            .finish()
    }
}

pub fn redact_url(u: &str) -> String {
    match (u.find("://"), u.rfind('@')) {
        (Some(a), Some(b)) if b > a => format!("{}://***@{}", &u[..a], &u[b + 1..]),
        _ => u.to_string(),
    }
}

impl DbConfig {
    pub fn resolve(secrets: &Path) -> Self {
        let file = std::fs::read_to_string(secrets.join("db.env")).unwrap_or_default();
        let get = |k: &str| {
            std::env::var(k).ok().filter(|v| !v.is_empty()).or_else(|| {
                file.lines().find_map(|l| {
                    let v = l
                        .trim()
                        .trim_start_matches("export ")
                        .strip_prefix(k)?
                        .strip_prefix('=')?;
                    Some(v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty())
                })
            })
        };
        Self {
            url: get("CMSQ_DATABASE_URL"),
            migrate_url: get("CMSQ_DATABASE_MIGRATE_URL"),
            schema: get("CMSQ_DATABASE_SCHEMA"),
            max_conns: get("CMSQ_DATABASE_POOL")
                .and_then(|v| v.parse().ok())
                .unwrap_or(16),
            offline: std::env::var("CMSQ_DB").is_ok_and(|v| v == "sqlite"),
        }
    }
}

pub struct Store {
    pub db: Backend,
    cfg: AuditConfig,
    /// serializes anchor appends within this process (the flock covers other processes)
    anchor_mu: std::sync::Mutex<()>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TrialRow {
    pub id: i64,
    pub strategy_id: String,
    pub idea_key: String,
    pub params_hash: String,
    pub symbol: String,
    pub sharpe_daily: f64,
    pub positions: Vec<f64>,
    /// run was caught by the look-ahead probe: counts toward N, excluded from the SR variance
    pub lookahead: bool,
    pub run_kind: String,
    pub group_id: Option<String>,
    /// signed snapshot the run was scored on
    pub dataset_sha: String,
    /// evaluation bars (fixes the scoring window on that snapshot)
    pub bars: i64,
    /// Inclusive dates of the actual scored returns, not the warm-up window.
    pub score_start: String,
    pub score_end: String,
}

impl TrialRow {
    /// Research-selection identity for the DSR count (issue #7): code idea,
    /// canonical params, market, signed snapshot and scoring window, run kind and
    /// portfolio group. Exact reruns of one identity collapse; any change along a
    /// selection dimension (symbol, data, window, allocation, params) is a new trial.
    /// No correlation discount is applied: the count is the conservative upper bound.
    pub fn selection_key(&self) -> String {
        // An old row cannot prove which dates were scored. Preserve it and
        // count conservatively instead of inventing a window or deduping it.
        if self.score_start.is_empty() || self.score_end.is_empty() {
            return format!("legacy-trial:{}", self.id);
        }
        serde_json::to_string(&(
            &self.idea_key,
            &self.params_hash,
            &self.symbol,
            &self.dataset_sha,
            self.bars,
            &self.score_start,
            &self.score_end,
            &self.run_kind,
            &self.group_id,
        ))
        .expect("a tuple of strings and integers serializes")
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VaultTouch {
    pub strategy_id: String,
    pub idea_key: Option<String>,
    pub symbol: Option<String>,
    pub code_sha: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResearchRun {
    pub id: String,
    pub kind: String,
    pub strategy_id: Option<String>,
    pub spec: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub status: String,
    pub created_at: String,
    pub finished_at: Option<String>,
}

pub struct NewTrial<'a> {
    pub strategy_id: &'a str,
    pub job_id: &'a str,
    pub idea_key: &'a str,
    pub params_hash: &'a str,
    pub symbol: &'a str,
    pub dataset_sha: &'a str,
    pub bars: usize,
    pub score_start: &'a str,
    pub score_end: &'a str,
    pub sharpe_daily: f64,
    pub positions: &'a [f64],
    pub lookahead: bool,
    pub run_kind: &'a str,
    pub group_id: Option<&'a str>,
}

fn ts(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_default()
}
fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates on success.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    let h = if ok {
        String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(0)])
            .trim()
            .to_string()
    } else {
        String::new()
    };
    if h.is_empty() {
        "unknown".into()
    } else {
        h
    }
}

/// Process start time, used to tell a live owner from a reused PID. Linux:
/// starttime (clock ticks since boot) from /proc/<pid>/stat. Elsewhere: "" (no
/// reuse detection; a reused PID keeps a dead lease looking alive, which only
/// delays recovery and never fails a live run).
fn proc_start(pid: i64) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, rest)| rest.split_whitespace().nth(19).unwrap_or("").to_string())
        })
        .unwrap_or_default()
}

/// Lease of the current process: "host|pid|start".
pub fn run_lease() -> String {
    let pid = std::process::id() as i64;
    format!("{}|{pid}|{}", hostname(), proc_start(pid))
}

/// Liveness contract for a lease (issue #8): `None` when it belongs to another
/// host (never reaped from here; that host recovers it), else whether the owner
/// process is alive — same PID AND same start time (PID reuse => dead).
pub fn lease_alive(lease: &str) -> Option<bool> {
    let mut it = lease.splitn(3, '|');
    let (host, pid, start) = (
        it.next()?,
        it.next()?.parse::<i64>().ok()?,
        it.next().unwrap_or(""),
    );
    if host != hostname() {
        return None;
    }
    Some(
        pid_alive(pid)
            && (start.is_empty() || proc_start(pid).is_empty() || proc_start(pid) == start),
    )
}

const ACOLS: &str = "seq,ts,actor,action,target,details,success,prev_hash,hash";
const SCOLS: &str = "id,title,source_site,source_url,author,scraped_at,ingested_at,raw_path,content_sha256,sample,status,attempts,last_reason,extracted_json,updated_at";
const JCOLS: &str = "id,strategy_id,backend,image,symbol,data_source,status,exit_code,started_at,finished_at,duration_ms,job_dir,error,result_json,owner_pid,runner_identity,kind,owner_host";
const TCOLS: &str = "id,strategy_id,idea_key,params_hash,symbol,sharpe_daily,positions_json,lookahead,run_kind,group_id,dataset_sha,bars,score_start,score_end";

fn audit_row(r: &R) -> AuditEntry {
    let d = r.s(5);
    AuditEntry {
        seq: r.i(0),
        ts: r.s(1),
        actor: r.s(2),
        action: r.s(3),
        target: r.os(4),
        details: serde_json::from_str(&d).unwrap_or(serde_json::Value::String(d)),
        success: r.b(6),
        prev_hash: r.s(7),
        hash: r.s(8),
    }
}
fn audit_msg(r: &R) -> Vec<u8> {
    audit::entry_message(
        &r.s(7),
        r.i(0),
        &r.s(1),
        &r.s(2),
        &r.s(3),
        r.os(4).as_deref(),
        &r.s(5),
        r.b(6),
    )
}
fn strategy_row(r: &R) -> Strategy {
    Strategy {
        id: r.s(0),
        title: r.s(1),
        source_site: r.s(2),
        source_url: r.os(3),
        author: r.os(4),
        scraped_at: r.os(5),
        ingested_at: ts(r.s(6)),
        raw_path: r.s(7),
        content_sha256: r.s(8),
        sample: r.b(9),
        status: StrategyStatus::parse(&r.s(10)).unwrap_or(StrategyStatus::Failed),
        attempts: r.i(11),
        last_reason: r.os(12),
        extracted: r.os(13).and_then(|e| serde_json::from_str(&e).ok()),
        updated_at: ts(r.s(14)),
    }
}
fn job_row(r: &R) -> JobRecord {
    JobRecord {
        id: r.s(0),
        strategy_id: r.s(1),
        backend: r.s(2),
        image: r.s(3),
        symbol: r.s(4),
        data_source: r.s(5),
        status: r.s(6),
        exit_code: r.oi(7),
        started_at: ts(r.s(8)),
        finished_at: r.os(9).map(ts),
        duration_ms: r.oi(10),
        job_dir: r.s(11),
        error: r.os(12),
        result: r.os(13).and_then(|s| serde_json::from_str(&s).ok()),
        owner_pid: r.oi(14),
        runner_identity: r.os(15),
        kind: r.s(16),
        owner_host: r.os(17),
    }
}
fn trial_row(r: &R) -> TrialRow {
    TrialRow {
        id: r.i(0),
        strategy_id: r.s(1),
        idea_key: r.s(2),
        params_hash: r.s(3),
        symbol: r.s(4),
        sharpe_daily: r.f(5),
        positions: serde_json::from_str(&r.s(6)).unwrap_or_default(),
        lookahead: r.b(7),
        run_kind: r.s(8),
        group_id: r.os(9),
        dataset_sha: r.s(10),
        bars: r.i(11),
        score_start: r.s(12),
        score_end: r.s(13),
    }
}

/// Exclusive advisory lock on `<anchor>.lock` for the duration of the guard.
struct FileLock(std::fs::File);
impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("lock"))?;
        // SAFETY: valid fd owned by `f`
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        anyhow::ensure!(
            rc == 0,
            "flock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        Ok(Self(f))
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: valid fd
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Store {
    // ---------- opening ----------
    /// Primary entry point: PostgreSQL when configured, SQLite only in explicit
    /// offline mode or when no database is configured (logged as offline).
    pub fn open_configured(sqlite_path: &Path, db: &DbConfig, cfg: AuditConfig) -> Result<Self> {
        match (&db.url, db.offline) {
            (Some(url), false) => Self::open_pg(
                url,
                db.migrate_url.as_deref(),
                db.schema.as_deref(),
                db.max_conns,
                cfg,
            ),
            _ => Self::open(sqlite_path, cfg),
        }
    }

    pub fn open_pg(
        url: &str,
        migrate_url: Option<&str>,
        schema: Option<&str>,
        max_conns: u32,
        cfg: AuditConfig,
    ) -> Result<Self> {
        if let Some(mu) = migrate_url {
            Self::migrate_pg(mu, schema)?;
        }
        let pg = Pg::connect(url, schema, max_conns.max(2))?;
        let s = Self {
            db: Backend::Postgres(pg),
            cfg,
            anchor_mu: Default::default(),
        };
        // schema must exist (migrations are run by the owner role)
        s.db.run(|e| e.query("SELECT 1 FROM audit LIMIT 1", &[]))
            .context("PostgreSQL schema missing: run `cmsq db migrate` (owner URL) first")?;
        Ok(s)
    }

    /// Apply embedded migrations with the owner role.
    pub fn migrate_pg(owner_url: &str, schema: Option<&str>) -> Result<()> {
        let pg = Pg::connect(owner_url, schema, 2)?;
        if let Some(sc) = schema {
            anyhow::ensure!(
                sc.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "bad schema name"
            );
            let q = format!("CREATE SCHEMA IF NOT EXISTS {sc}; DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='cmsq_app') THEN GRANT USAGE ON SCHEMA {sc} TO cmsq_app; END IF; END $$;");
            pg.block(async { sqlx::raw_sql(&q).execute(&pg.pool).await.map(|_| ()) })?;
        }
        pg.block(async { PG_MIGRATOR.run(&pg.pool).await })
            .context("PostgreSQL migrations")?;
        Ok(())
    }

    pub fn open(path: &Path, cfg: AuditConfig) -> Result<Self> {
        let conn =
            rusqlite::Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        Self::sqlite_migrate(&conn)?;
        Ok(Self {
            db: Backend::Sqlite(std::sync::Mutex::new(conn)),
            cfg,
            anchor_mu: Default::default(),
        })
    }
    fn sqlite_migrate(conn: &rusqlite::Connection) -> Result<()> {
        conn.execute_batch(SQLITE_SCHEMA)?;
        for (t, col, ddl) in SQLITE_COLUMNS {
            let has: bool = conn
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{t}') WHERE name=?1"
                ))?
                .exists([col])?;
            if !has {
                conn.execute_batch(&format!("ALTER TABLE {t} ADD COLUMN {col} {ddl}"))?;
            }
        }
        conn.execute_batch(SQLITE_LATE_TRIGGERS)?;
        Ok(())
    }
    /// Read-side open for viewers (dashboard): never runs migrations; PostgreSQL uses
    /// the runtime role with a small pool, SQLite opens the file read-only.
    pub fn open_configured_readonly(
        sqlite_path: &Path,
        db: &DbConfig,
        cfg: AuditConfig,
    ) -> Result<Self> {
        match (&db.url, db.offline) {
            (Some(url), false) => Self::open_pg(url, None, db.schema.as_deref(), 2, cfg),
            _ => Self::open_readonly(sqlite_path, cfg),
        }
    }
    pub fn open_readonly(path: &Path, cfg: AuditConfig) -> Result<Self> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            db: Backend::Sqlite(std::sync::Mutex::new(conn)),
            cfg,
            anchor_mu: Default::default(),
        })
    }
    pub fn in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Self::sqlite_migrate(&conn)?;
        Ok(Self {
            db: Backend::Sqlite(std::sync::Mutex::new(conn)),
            cfg: AuditConfig::insecure_test(),
            anchor_mu: Default::default(),
        })
    }
    pub fn backend(&self) -> &'static str {
        self.db.name()
    }
    /// Raw SQL batch. Used by migrations and by tests that play the attacker.
    #[doc(hidden)]
    pub fn raw_batch(&self, sql: &str) -> Result<()> {
        self.db.batch(sql)
    }
    #[doc(hidden)]
    pub fn raw_exec(&self, sql: &str, params: &[V]) -> Result<u64> {
        self.db.run(|e| e.exec(sql, params))
    }

    // ---------- audit ----------
    pub fn audit(
        &self,
        actor: &str,
        action: &str,
        target: Option<&str>,
        details: serde_json::Value,
        success: bool,
    ) -> Result<AuditEntry> {
        let d = serde_json::to_string(&details)?;
        let _g = self.anchor_mu.lock().unwrap_or_else(|e| e.into_inner());
        let _fl = match &self.cfg.anchor {
            Some(a) => Some(FileLock::acquire(a)?),
            None => None,
        };
        let cfg = self.cfg.clone();
        let (seq, ts, prev_hash, hash) = self.db.tx(Some(Lock::Audit), |e| {
            let (prev_seq, prev_hash) = e.one("SELECT seq, hash FROM audit ORDER BY seq DESC LIMIT 1", &[])?
                .map(|r| (r.i(0), r.s(1))).unwrap_or((0, GENESIS.to_string()));
            let seq = prev_seq + 1;
            let ts = now();
            let hash = cfg.mac_next(seq, &prev_hash, &audit::entry_message(&prev_hash, seq, &ts, actor, action, target, &d, success))?;
            e.exec("INSERT INTO audit(seq,ts,actor,action,target,details,success,prev_hash,hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                p![seq, ts.as_str(), actor, action, target, d.as_str(), success, prev_hash.as_str(), hash.as_str()])?;
            Ok((seq, ts, prev_hash, hash))
        })?;
        if let Some(a) = &self.cfg.anchor {
            audit::append_anchor(a, seq, &hash, &ts)
                .with_context(|| format!("write audit anchor {}", a.display()))?;
        }
        Ok(AuditEntry {
            seq,
            ts,
            actor: actor.into(),
            action: action.into(),
            target: target.map(Into::into),
            details,
            success,
            prev_hash,
            hash,
        })
    }

    pub fn audit_entries(&self, target: Option<&str>, limit: i64) -> Result<Vec<AuditEntry>> {
        let sql = format!(
            "SELECT {ACOLS} FROM audit WHERE (?1 IS NULL OR target=?1) ORDER BY seq DESC LIMIT ?2"
        );
        Ok(self
            .db
            .run(|e| e.query(&sql, p![target, limit]))?
            .iter()
            .map(audit_row)
            .collect())
    }

    /// Recompute the whole chain. Detects edits, deletions (seq gaps) and re-ordering.
    pub fn verify_audit(&self) -> Result<VerifyReport> {
        let rows = self
            .db
            .run(|e| e.query(&format!("SELECT {ACOLS} FROM audit ORDER BY seq ASC"), &[]))?;
        let mut prev = GENESIS.to_string();
        let mut expect_seq = 1;
        let mut n = 0;
        let anchor = self.cfg.anchor.as_ref().map(|a| a.display().to_string());
        for r in &rows {
            let (seq, prev_hash, hash) = (r.i(0), r.s(7), r.s(8));
            n += 1;
            let bad = |p: String| VerifyReport {
                entries: n,
                ok: false,
                first_bad_seq: Some(seq),
                problem: Some(p),
                head_hash: hash.clone(),
                keyed: true,
                anchor: anchor.clone(),
                anchored_seq: None,
            };
            if seq != expect_seq {
                return Ok(bad(format!(
                    "sequence gap: expected {expect_seq}, found {seq} (entry deleted?)"
                )));
            }
            if prev_hash != prev {
                return Ok(bad("prev_hash does not link to previous entry".into()));
            }
            if !self.cfg.check(seq, &audit_msg(r), &hash)? {
                if seq == 1 {
                    let why = "this workspace's audit chain was not signed with the current key: it was created by an older cmsq (unkeyed sha256 chain) or with a different audit.key / CMSQ_SECRETS_DIR. Use a fresh CMSQ_HOME, or point CMSQ_SECRETS_DIR at the key that created it";
                    return Ok(bad(format!(
                        "HMAC mismatch at the first entry: entry 1 was modified, or {why}"
                    )));
                }
                return Ok(bad(
                    "HMAC mismatch (entry modified or chain re-computed without the key)".into(),
                ));
            }
            prev = hash;
            expect_seq += 1;
        }
        let head_seq = expect_seq - 1;
        let mut rep = VerifyReport {
            entries: n,
            ok: true,
            first_bad_seq: None,
            problem: None,
            head_hash: prev.clone(),
            keyed: true,
            anchor,
            anchored_seq: None,
        };
        if let Some(a) = &self.cfg.anchor {
            match audit::read_anchor(a)? {
                None if head_seq > 0 => {
                    rep.ok = false;
                    rep.problem = Some(format!(
                        "anchor file {} is missing but the log has {head_seq} entries",
                        a.display()
                    ));
                }
                None => {}
                Some((aseq, ahash)) => {
                    rep.anchored_seq = Some(aseq);
                    if aseq > head_seq {
                        rep.ok = false;
                        rep.first_bad_seq = Some(head_seq + 1);
                        rep.problem = Some(format!("log truncated: anchor says head is seq {aseq}, database ends at {head_seq}"));
                    } else {
                        let h = rows
                            .get((aseq - 1).max(0) as usize)
                            .filter(|r| r.i(0) == aseq)
                            .map(|r| r.s(8));
                        if h.as_deref() != Some(ahash.as_str()) {
                            rep.ok = false;
                            rep.first_bad_seq = Some(aseq);
                            rep.problem = Some(format!("anchored head seq {aseq} does not match the database (log rewritten or replaced)"));
                        } else if aseq < head_seq {
                            rep.ok = false;
                            rep.first_bad_seq = Some(aseq + 1);
                            rep.problem = Some(format!("entries after seq {aseq} are not anchored (appended by something other than cmsq?)"));
                        }
                    }
                }
            }
        }
        // the separate signer remembers its own head: a second, independent anchor
        if rep.ok {
            if let Some((sseq, smac)) = self.cfg.signer_head()? {
                let at = rows
                    .get((sseq - 1).max(0) as usize)
                    .filter(|r| r.i(0) == sseq)
                    .map(|r| r.s(8));
                if head_seq < sseq - 1 || (head_seq >= sseq && at.as_deref() != Some(smac.as_str()))
                {
                    rep.ok = false;
                    rep.first_bad_seq = Some(sseq.min(head_seq + 1));
                    rep.problem = Some(format!("audit signer head seq {sseq} does not match the database (truncated or rewritten)"));
                }
            }
        }
        Ok(rep)
    }

    /// Check external git anchors against the database: every anchored (seq, mac) must
    /// still be in the log. Returns (anchors checked, problems).
    pub fn verify_git_anchors(&self, repo: &Path) -> Result<(usize, Vec<String>)> {
        let hist = crate::gitanchor::history(repo, &self.cfg.chain)?;
        let mut bad = vec![];
        for (seq, mac, ts, commit) in &hist {
            let r = self
                .db
                .run(|e| e.one("SELECT hash FROM audit WHERE seq=?1", p![*seq]))?;
            match r.map(|r| r.s(0)) {
                Some(h) if &h == mac => {}
                Some(_) => bad.push(format!("seq {seq} anchored at {ts} (commit {}) has a different MAC now: history rewritten", &commit[..8])),
                None => bad.push(format!("seq {seq} anchored at {ts} (commit {}) is missing: log truncated", &commit[..8])),
            }
        }
        Ok((hist.len(), bad))
    }
    pub fn audit_chain_id(&self) -> &str {
        &self.cfg.chain
    }
    pub fn audit_key_source(&self) -> String {
        self.cfg.key_source()
    }

    // ---------- strategies ----------
    pub fn insert_strategy(&self, s: &Strategy) -> Result<()> {
        let ex = s
            .extracted
            .as_ref()
            .map(|e| serde_json::to_string(e).unwrap());
        self.db.run(|e| e.exec(&format!("INSERT INTO strategies({SCOLS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"),
            p![&s.id, &s.title, &s.source_site, &s.source_url, &s.author, &s.scraped_at, s.ingested_at.to_rfc3339(), &s.raw_path,
                &s.content_sha256, s.sample, s.status.as_str(), s.attempts, &s.last_reason, ex, s.updated_at.to_rfc3339()]))?;
        Ok(())
    }
    pub fn find_by_sha(&self, sha: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .run(|e| e.one("SELECT id FROM strategies WHERE content_sha256=?1", p![sha]))?
            .map(|r| r.s(0)))
    }
    pub fn get_strategy(&self, id: &str) -> Result<Strategy> {
        let sql = format!(
            "SELECT {SCOLS} FROM strategies WHERE id=?1 OR id LIKE ?1 || '%' ORDER BY id LIMIT 2"
        );
        let v = self.db.run(|e| e.query(&sql, p![id]))?;
        match v.len() {
            0 => anyhow::bail!("strategy not found: {id}"),
            1 => Ok(strategy_row(&v[0])),
            _ => anyhow::bail!("ambiguous strategy id prefix: {id}"),
        }
    }
    pub fn list_strategies(&self, status: Option<StrategyStatus>) -> Result<Vec<Strategy>> {
        let sql = format!("SELECT {SCOLS} FROM strategies WHERE (?1 IS NULL OR status=?1) ORDER BY ingested_at, id");
        Ok(self
            .db
            .run(|e| e.query(&sql, p![status.map(|s| s.as_str())]))?
            .iter()
            .map(strategy_row)
            .collect())
    }
    /// Unconditional status write (crash recovery / tests). Pipeline code uses [`Self::cas_status`].
    pub fn set_status(&self, id: &str, to: StrategyStatus, reason: Option<&str>) -> Result<()> {
        self.db.run(|e| {
            e.exec(
                "UPDATE strategies SET status=?2, last_reason=?3, updated_at=?4 WHERE id=?1",
                p![id, to.as_str(), reason, now()],
            )
        })?;
        Ok(())
    }
    /// Compare-and-swap: only moves `from -> to`; false if someone else moved it first.
    pub fn cas_status(
        &self,
        id: &str,
        from: StrategyStatus,
        to: StrategyStatus,
        reason: Option<&str>,
    ) -> Result<bool> {
        // Entering Backtesting takes the run lease in the SAME statement as the
        // status change; leaving it releases the lease (issue #8).
        let lease = (to == StrategyStatus::Backtesting).then(run_lease);
        Ok(self.db.run(|e| e.exec("UPDATE strategies SET status=?3, last_reason=?4, updated_at=?5, run_owner=?6 WHERE id=?1 AND status=?2",
            p![id, from.as_str(), to.as_str(), reason, now(), lease]))? == 1)
    }
    /// Test hook: overwrite a lease (simulates an owner on another host / a dead PID).
    #[doc(hidden)]
    pub fn set_run_owner_for_test(&self, id: &str, lease: Option<&str>) -> Result<()> {
        self.db.run(|e| {
            e.exec(
                "UPDATE strategies SET run_owner=?2 WHERE id=?1",
                p![id, lease],
            )
        })?;
        Ok(())
    }
    /// Run lease ("host|pid|proc-start") of a strategy, if any.
    pub fn run_owner(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .run(|e| e.one("SELECT run_owner FROM strategies WHERE id=?1", p![id]))?
            .and_then(|r| r.os(0)))
    }
    /// Recovery CAS: Backtesting -> Failed only if the lease is still exactly the
    /// one the caller judged dead (`None` = legacy row without a lease).
    pub fn reap_backtesting(&self, id: &str, lease: Option<&str>, reason: &str) -> Result<bool> {
        Ok(self.db.run(|e| match lease {
            Some(l) => e.exec("UPDATE strategies SET status='Failed', last_reason=?3, updated_at=?4, run_owner=NULL WHERE id=?1 AND status='Backtesting' AND run_owner=?2", p![id, l, reason, now()]),
            None => e.exec("UPDATE strategies SET status='Failed', last_reason=?2, updated_at=?3 WHERE id=?1 AND status='Backtesting' AND run_owner IS NULL", p![id, reason, now()]),
        })? == 1)
    }
    /// Recovery CAS for a job: only a job still `running` is abandoned, so a
    /// concurrent finish_job is never overwritten.
    pub fn abandon_job(&self, id: &str, reason: &str) -> Result<bool> {
        Ok(self.db.run(|e| e.exec("UPDATE jobs SET status='abandoned', finished_at=?2, duration_ms=0, error=?3 WHERE id=?1 AND status='running'", p![id, now(), reason]))? == 1)
    }
    pub fn set_extracted(&self, id: &str, x: &ExtractedStrategy) -> Result<()> {
        let j = serde_json::to_string(x)?;
        self.db.run(|e| {
            e.exec(
                "UPDATE strategies SET extracted_json=?2, updated_at=?3 WHERE id=?1",
                p![id, j, now()],
            )
        })?;
        Ok(())
    }
    pub fn bump_attempts(&self, id: &str) -> Result<i64> {
        self.db.tx(None, |e| {
            e.exec(
                "UPDATE strategies SET attempts=attempts+1 WHERE id=?1",
                p![id],
            )?;
            Ok(
                e.one("SELECT attempts FROM strategies WHERE id=?1", p![id])?
                    .map(|r| r.i(0))
                    .unwrap_or(0),
            )
        })
    }

    // ---------- jobs ----------
    pub fn insert_job(&self, j: &JobRecord) -> Result<()> {
        self.db.run(|e| e.exec("INSERT INTO jobs(id,strategy_id,backend,image,symbol,data_source,status,started_at,job_dir,owner_pid,runner_identity,kind,owner_host) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            p![&j.id, &j.strategy_id, &j.backend, &j.image, &j.symbol, &j.data_source, &j.status, j.started_at.to_rfc3339(), &j.job_dir,
                std::process::id() as i64, &j.runner_identity, &j.kind, hostname()]))?;
        Ok(())
    }
    pub fn finish_job(
        &self,
        id: &str,
        status: &str,
        exit_code: Option<i64>,
        duration_ms: i64,
        error: Option<&str>,
        result: Option<&serde_json::Value>,
    ) -> Result<()> {
        self.db.run(|e| e.exec("UPDATE jobs SET status=?2, exit_code=?3, finished_at=?4, duration_ms=?5, error=?6, result_json=?7 WHERE id=?1",
            p![id, status, exit_code, now(), duration_ms, error, result.map(|r| r.to_string())]))?;
        Ok(())
    }
    pub fn list_jobs(&self, strategy_id: Option<&str>) -> Result<Vec<JobRecord>> {
        let sql = format!("SELECT {JCOLS} FROM jobs WHERE (?1 IS NULL OR strategy_id=?1) ORDER BY started_at DESC");
        Ok(self
            .db
            .run(|e| e.query(&sql, p![strategy_id]))?
            .iter()
            .map(job_row)
            .collect())
    }
    /// Jobs left in `running` by a process on THIS host that is gone (crash / kill -9).
    /// Jobs owned by other hosts sharing the database are never reaped from here.
    pub fn stale_running_jobs(&self) -> Result<Vec<JobRecord>> {
        let me = hostname();
        Ok(self
            .list_jobs(None)?
            .into_iter()
            .filter(|j| {
                j.status == "running"
                    && j.owner_host.as_deref().is_none_or(|h| h == me)
                    && !j.owner_pid.is_some_and(pid_alive)
            })
            .collect())
    }

    // ---------- trials (library-wide multiple-testing ledger) ----------
    /// Insert a trial and return the whole ledger as of (and including) this
    /// insert, atomically: concurrent workers each see a distinct, monotone N.
    pub fn record_trial(&self, t: &NewTrial) -> Result<Vec<TrialRow>> {
        let pos = serde_json::to_string(t.positions)?;
        self.db.tx(Some(Lock::Ledger), |e| {
            e.exec("INSERT INTO trials(strategy_id,job_id,idea_key,params_hash,symbol,dataset_sha,bars,sharpe_daily,positions_json,created_at,lookahead,run_kind,group_id,score_start,score_end) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                p![t.strategy_id, t.job_id, t.idea_key, t.params_hash, t.symbol, t.dataset_sha, t.bars, t.sharpe_daily, pos.as_str(), now(), t.lookahead, t.run_kind, t.group_id, t.score_start, t.score_end])?;
            Ok(e.query(&format!("SELECT {TCOLS} FROM trials ORDER BY id"), &[])?.iter().map(trial_row).collect())
        })
    }
    /// P0-compatible wrapper.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_trial(
        &self,
        sid: &str,
        job: &str,
        idea_key: &str,
        params_hash: &str,
        symbol: &str,
        dataset_sha: &str,
        bars: usize,
        sr: f64,
        positions: &[f64],
        lookahead: bool,
    ) -> Result<()> {
        self.record_trial(&NewTrial {
            strategy_id: sid,
            job_id: job,
            idea_key,
            params_hash,
            symbol,
            dataset_sha,
            bars,
            score_start: "",
            score_end: "",
            sharpe_daily: sr,
            positions,
            lookahead,
            run_kind: "single",
            group_id: None,
        })?;
        Ok(())
    }
    pub fn trials(&self) -> Result<Vec<TrialRow>> {
        Ok(self
            .db
            .run(|e| e.query(&format!("SELECT {TCOLS} FROM trials ORDER BY id"), &[]))?
            .iter()
            .map(trial_row)
            .collect())
    }
    pub fn trial_count(&self) -> Result<i64> {
        Ok(self
            .db
            .run(|e| e.one("SELECT COUNT(*) FROM trials", &[]))?
            .map(|r| r.i(0))
            .unwrap_or(0))
    }

    // ---------- vault (final holdout, one touch per idea) ----------
    pub fn vault_touch(&self, sid: &str) -> Result<Option<(String, bool)>> {
        Ok(self
            .db
            .run(|e| {
                e.one(
                    "SELECT job_id, passed FROM vault_touches WHERE strategy_id=?1",
                    p![sid],
                )
            })?
            .map(|r| (r.s(0), r.b(1))))
    }
    /// Atomically: read every previous vault touch + the trial ledger, run
    /// `conflict` (dedup by idea / code / correlated positions), and only if it
    /// returns `None` record the touch. Two concurrent promotes of the same idea
    /// cannot both get through.
    #[allow(clippy::too_many_arguments)]
    pub fn claim_vault(
        &self,
        sid: &str,
        job: &str,
        idea_key: &str,
        symbol: &str,
        code_sha: &str,
        conflict: impl FnOnce(&[VaultTouch], &[TrialRow]) -> Option<String>,
    ) -> Result<std::result::Result<(), String>> {
        self.db.tx(Some(Lock::Vault), |e| {
            if let Some(r) = e.one("SELECT job_id FROM vault_touches WHERE strategy_id=?1", p![sid])? {
                return Ok(Err(format!("vault already used (job {})", r.s(0))));
            }
            let touches: Vec<VaultTouch> = e.query("SELECT strategy_id, idea_key, symbol, code_sha FROM vault_touches ORDER BY created_at", &[])?
                .iter().map(|r| VaultTouch { strategy_id: r.s(0), idea_key: r.os(1), symbol: r.os(2), code_sha: r.os(3) }).collect();
            let trials: Vec<TrialRow> = e.query(&format!("SELECT {TCOLS} FROM trials ORDER BY id"), &[])?.iter().map(trial_row).collect();
            if let Some(why) = conflict(&touches, &trials) {
                return Ok(Err(why));
            }
            e.exec("INSERT INTO vault_touches(strategy_id,job_id,passed,created_at,idea_key,symbol,code_sha,finished) VALUES(?1,?2,0,?3,?4,?5,?6,0)",
                p![sid, job, now(), idea_key, symbol, code_sha])?;
            Ok(Ok(()))
        })
    }
    /// Write the vault verdict once; the row is then frozen by trigger.
    pub fn finish_vault(&self, sid: &str, passed: bool) -> Result<()> {
        let n = self.db.run(|e| e.exec("UPDATE vault_touches SET passed=?2, finished=1 WHERE strategy_id=?1 AND finished=0", p![sid, passed]))?;
        anyhow::ensure!(n == 1, "vault touch for {sid} missing or already finalized");
        Ok(())
    }
    pub fn record_vault_touch(&self, sid: &str, job: &str, passed: bool) -> Result<()> {
        self.record_vault_touch_keyed(sid, job, passed, None, None, None)
    }
    pub fn record_vault_touch_keyed(
        &self,
        sid: &str,
        job: &str,
        passed: bool,
        idea_key: Option<&str>,
        symbol: Option<&str>,
        code_sha: Option<&str>,
    ) -> Result<()> {
        self.db.tx(Some(Lock::Vault), |e| e.exec("INSERT INTO vault_touches(strategy_id,job_id,passed,created_at,idea_key,symbol,code_sha,finished) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
            p![sid, job, passed, now(), idea_key, symbol, code_sha]))?;
        Ok(())
    }
    pub fn vault_touches(&self) -> Result<Vec<VaultTouch>> {
        Ok(self.db.run(|e| e.query("SELECT strategy_id, idea_key, symbol, code_sha FROM vault_touches ORDER BY created_at", &[]))?
            .iter().map(|r| VaultTouch { strategy_id: r.s(0), idea_key: r.os(1), symbol: r.os(2), code_sha: r.os(3) }).collect())
    }

    /// Latest successful `market_data_fetch` audit entry for `symbol`, with its
    /// own HMAC re-verified (an edited row is refused).
    pub fn latest_fetch_attestation(&self, symbol: &str) -> Result<Option<AuditEntry>> {
        let r = self.db.run(|e| e.one(&format!("SELECT {ACOLS} FROM audit WHERE action='market_data_fetch' AND target=?1 AND success=1 ORDER BY seq DESC LIMIT 1"), p![symbol]))?;
        let Some(r) = r else { return Ok(None) };
        anyhow::ensure!(
            self.cfg.check(r.i(0), &audit_msg(&r), &r.s(8))?,
            crate::error::CoreError::AuditIntegrity(format!(
                "data attestation entry seq {} fails its HMAC",
                r.i(0)
            ))
        );
        let mut a = audit_row(&r);
        a.details = serde_json::from_str(&r.s(5))?;
        Ok(Some(a))
    }

    // ---------- evaluations / signals ----------
    pub fn insert_evaluation(&self, x: &Evaluation) -> Result<()> {
        let reasons = serde_json::to_string(&x.reasons)?;
        self.db.run(|e| e.exec("INSERT INTO evaluations(id,strategy_id,job_id,passed,reasons_json,checks_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            p![&x.id, &x.strategy_id, &x.job_id, x.passed, reasons, x.checks.to_string(), x.created_at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)]))?;
        Ok(())
    }
    pub fn latest_evaluation(&self, sid: &str) -> Result<Option<Evaluation>> {
        Ok(self.db.run(|e| e.one("SELECT id,strategy_id,job_id,passed,reasons_json,checks_json,created_at FROM evaluations WHERE strategy_id=?1 ORDER BY created_at DESC, id DESC LIMIT 1", p![sid]))?
            .map(|r| Evaluation { id: r.s(0), strategy_id: r.s(1), job_id: r.s(2), passed: r.b(3), reasons: serde_json::from_str(&r.s(4)).unwrap_or_default(),
                checks: serde_json::from_str(&r.s(5)).unwrap_or_default(), created_at: ts(r.s(6)) }))
    }
    pub fn insert_signal(&self, s: &Signal) -> Result<()> {
        self.db.run(|e| e.exec("INSERT INTO signals(strategy_id,job_id,symbol,date,target_position,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            p![&s.strategy_id, &s.job_id, &s.symbol, &s.date, s.target_position, s.created_at.to_rfc3339()]))?;
        Ok(())
    }
    pub fn list_signals(&self) -> Result<Vec<(Signal, String, String)>> {
        Ok(self.db.run(|e| e.query("SELECT g.strategy_id,g.job_id,g.symbol,g.date,g.target_position,g.created_at,s.title,s.status FROM signals g JOIN strategies s ON s.id=g.strategy_id ORDER BY g.id DESC", &[]))?
            .iter().map(|r| (Signal { strategy_id: r.s(0), job_id: r.s(1), symbol: r.s(2), date: r.s(3), target_position: r.f(4), created_at: ts(r.s(5)) }, r.s(6), r.s(7))).collect())
    }
    pub fn counts(&self) -> Result<Vec<(String, i64)>> {
        Ok(self
            .db
            .run(|e| {
                e.query(
                    "SELECT status, COUNT(*) FROM strategies GROUP BY status",
                    &[],
                )
            })?
            .iter()
            .map(|r| (r.s(0), r.i(1)))
            .collect())
    }
    pub fn total_attempts(&self) -> Result<i64> {
        Ok(self
            .db
            .run(|e| {
                e.one(
                    "SELECT CAST(COALESCE(SUM(attempts),0) AS BIGINT) FROM strategies",
                    &[],
                )
            })?
            .map(|r| r.i(0))
            .unwrap_or(0))
    }

    // ---------- research runs (sweeps, portfolios) ----------
    pub fn insert_run(&self, r: &ResearchRun) -> Result<()> {
        self.db.run(|e| e.exec("INSERT INTO research_runs(id,kind,strategy_id,spec_json,result_json,status,created_at,finished_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            p![&r.id, &r.kind, &r.strategy_id, r.spec.to_string(), r.result.as_ref().map(|v| v.to_string()), &r.status, &r.created_at, &r.finished_at]))?;
        Ok(())
    }
    pub fn finish_run(&self, id: &str, status: &str, result: &serde_json::Value) -> Result<()> {
        self.db.run(|e| {
            e.exec(
                "UPDATE research_runs SET status=?2, result_json=?3, finished_at=?4 WHERE id=?1",
                p![id, status, result.to_string(), now()],
            )
        })?;
        Ok(())
    }
    // ---------- paper trading (simulated; cmsq-paper) ----------
    pub fn create_paper_session(
        &self,
        id: &str,
        strategy_id: &str,
        config: &serde_json::Value,
    ) -> Result<()> {
        let t = now();
        self.db.run(|e| e.exec("INSERT INTO paper_sessions(id,strategy_id,config_json,status,created_at,updated_at) VALUES(?1,?2,?3,'running',?4,?4)",
            p![id, strategy_id, config.to_string(), &t]))?;
        Ok(())
    }
    pub fn set_paper_session_status(&self, id: &str, status: &str) -> Result<()> {
        self.db.run(|e| {
            e.exec(
                "UPDATE paper_sessions SET status=?2, updated_at=?3 WHERE id=?1",
                p![id, status, now()],
            )
        })?;
        Ok(())
    }
    /// (id, strategy_id, config, status, created_at)
    #[allow(clippy::type_complexity)]
    pub fn list_paper_sessions(
        &self,
    ) -> Result<Vec<(String, String, serde_json::Value, String, String)>> {
        Ok(self.db.run(|e| e.query("SELECT id,strategy_id,config_json,status,created_at FROM paper_sessions ORDER BY created_at DESC", p![]))?
            .iter().map(|r| (r.s(0), r.s(1), serde_json::from_str(&r.s(2)).unwrap_or_default(), r.s(3), r.s(4))).collect())
    }
    #[allow(clippy::explicit_counter_loop)]
    /// Append events (seq, kind, json, chain) in one transaction; seq must continue the stored log.
    pub fn append_paper_events(
        &self,
        session: &str,
        evs: &[(i64, String, serde_json::Value, String)],
    ) -> Result<()> {
        if evs.is_empty() {
            return Ok(());
        }
        let t = now();
        self.db.tx(None, |e| {
            let last = e.query("SELECT COALESCE(MAX(seq),0) FROM paper_events WHERE session_id=?1", p![session])?;
            let mut want = last.first().map(|r| r.i(0)).unwrap_or(0) + 1;
            for (seq, kind, j, chain) in evs {
                anyhow::ensure!(*seq == want, "paper event gap: expected seq {want}, got {seq}");
                e.exec("INSERT INTO paper_events(session_id,seq,kind,event_json,chain,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                    p![session, *seq, kind, j.to_string(), chain, &t])?;
                want += 1;
            }
            Ok(())
        })?;
        Ok(())
    }
    /// (seq, kind, event, chain), ascending
    pub fn list_paper_events(
        &self,
        session: &str,
        after: i64,
        limit: i64,
    ) -> Result<Vec<(i64, String, serde_json::Value, String)>> {
        Ok(self.db.run(|e| e.query("SELECT seq,kind,event_json,chain FROM paper_events WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3", p![session, after, limit]))?
            .iter().map(|r| (r.i(0), r.s(1), serde_json::from_str(&r.s(2)).unwrap_or_default(), r.s(3))).collect())
    }
    pub fn insert_paper_snapshot(
        &self,
        session: &str,
        seq: i64,
        trading_day: Option<&str>,
        equity: f64,
        state: &serde_json::Value,
    ) -> Result<()> {
        self.db.run(|e| e.exec("INSERT INTO paper_snapshots(session_id,seq,trading_day,equity,state_json,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            p![session, seq, trading_day, equity, state.to_string(), now()]))?;
        Ok(())
    }
    /// latest (seq, state)
    pub fn latest_paper_snapshot(&self, session: &str) -> Result<Option<(i64, serde_json::Value)>> {
        Ok(self.db.run(|e| e.query("SELECT seq,state_json FROM paper_snapshots WHERE session_id=?1 ORDER BY seq DESC LIMIT 1", p![session]))?
            .first().map(|r| (r.i(0), serde_json::from_str(&r.s(1)).unwrap_or_default())))
    }
    /// Raw snapshot rows `(seq, trading_day, equity, state)`. **Unauthenticated**:
    /// `trading_day`/`equity` are derived columns a DB owner can edit (issue #38).
    /// Display code must go through `cmsq_paper::session::verified_equity_curve`.
    #[allow(clippy::type_complexity)]
    pub fn paper_snapshot_rows(
        &self,
        session: &str,
    ) -> Result<Vec<(i64, Option<String>, f64, serde_json::Value)>> {
        Ok(self.db.run(|e| e.query("SELECT seq,trading_day,equity,state_json FROM paper_snapshots WHERE session_id=?1 ORDER BY seq", p![session]))?
            .iter().map(|r| (r.i(0), r.os(1), r.f(2), serde_json::from_str(&r.s(3)).unwrap_or_default())).collect())
    }

    pub fn list_runs(&self, kind: Option<&str>) -> Result<Vec<ResearchRun>> {
        Ok(self.db.run(|e| e.query("SELECT id,kind,strategy_id,spec_json,result_json,status,created_at,finished_at FROM research_runs WHERE (?1 IS NULL OR kind=?1) ORDER BY created_at DESC", p![kind]))?
            .iter().map(|r| ResearchRun { id: r.s(0), kind: r.s(1), strategy_id: r.os(2), spec: serde_json::from_str(&r.s(3)).unwrap_or_default(),
                result: r.os(4).and_then(|s| serde_json::from_str(&s).ok()), status: r.s(5), created_at: r.s(6), finished_at: r.os(7) }).collect())
    }

    // ---------- time series (TimescaleDB hypertables on PostgreSQL) ----------
    /// Mirror a verified snapshot into `bars` (query/dashboard use; trust still comes
    /// from the hashed snapshot file + audit attestation).
    pub fn insert_bars(
        &self,
        symbol: &str,
        snapshot_sha: &str,
        bars: &[crate::market::Bar],
    ) -> Result<usize> {
        let pgdate = matches!(self.db, Backend::Postgres(_));
        let sql = if pgdate {
            "INSERT INTO bars(symbol,date,snapshot_sha,open,high,low,close,volume,adj_factor) VALUES(?1,CAST(?2 AS DATE),?3,?4,?5,?6,?7,?8,?9) ON CONFLICT DO NOTHING"
        } else {
            "INSERT OR IGNORE INTO bars(symbol,date,snapshot_sha,open,high,low,close,volume,adj_factor) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)"
        };
        self.db.tx(None, |e| {
            let mut n = 0;
            for b in bars {
                n += e.exec(
                    sql,
                    p![
                        symbol,
                        b.date.as_str(),
                        snapshot_sha,
                        b.open,
                        b.high,
                        b.low,
                        b.close,
                        b.volume,
                        b.adj_factor
                    ],
                )? as usize;
            }
            Ok(n)
        })
    }
    pub fn bar_count(&self, symbol: &str) -> Result<i64> {
        Ok(self
            .db
            .run(|e| e.one("SELECT COUNT(*) FROM bars WHERE symbol=?1", p![symbol]))?
            .map(|r| r.i(0))
            .unwrap_or(0))
    }
    pub fn insert_ticks(&self, ticks: &[crate::ctp::Tick]) -> Result<usize> {
        let pg = matches!(self.db, Backend::Postgres(_));
        let sql = if pg {
            "INSERT INTO ticks(instrument,ts,trading_day,last,volume,open_interest,bid1,ask1,source) VALUES(?1,CAST(?2 AS TIMESTAMPTZ),?3,?4,?5,?6,?7,?8,?9)"
        } else {
            "INSERT INTO ticks(instrument,ts,trading_day,last,volume,open_interest,bid1,ask1,source) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)"
        };
        self.db.tx(None, |e| {
            for t in ticks {
                e.exec(
                    sql,
                    p![
                        &t.instrument,
                        &t.ts,
                        &t.trading_day,
                        t.last,
                        t.volume,
                        t.open_interest,
                        t.bid1,
                        t.ask1,
                        &t.source
                    ],
                )?;
            }
            Ok(ticks.len())
        })
    }
    /// Recorded ticks of one instrument in time order (paper-trading replay).
    pub fn list_ticks(&self, instrument: &str, limit: i64) -> Result<Vec<crate::ctp::Tick>> {
        let pg = matches!(self.db, Backend::Postgres(_));
        let sql = if pg {
            "SELECT instrument,CAST(ts AS TEXT),trading_day,last,volume,open_interest,bid1,ask1,source FROM ticks WHERE instrument=?1 ORDER BY ts LIMIT ?2"
        } else {
            "SELECT instrument,ts,trading_day,last,volume,open_interest,bid1,ask1,source FROM ticks WHERE instrument=?1 ORDER BY ts, rowid LIMIT ?2"
        };
        Ok(self
            .db
            .run(|e| e.query(sql, p![instrument, limit]))?
            .iter()
            .map(|r| crate::ctp::Tick {
                instrument: r.s(0),
                ts: r.s(1),
                trading_day: r.s(2),
                last: r.f(3),
                volume: r.f(4),
                open_interest: r.of(5),
                bid1: r.of(6),
                ask1: r.of(7),
                source: r.s(8),
            })
            .collect())
    }
    pub fn tick_count(&self, instrument: Option<&str>) -> Result<i64> {
        Ok(self
            .db
            .run(|e| {
                e.one(
                    "SELECT COUNT(*) FROM ticks WHERE (?1 IS NULL OR instrument=?1)",
                    p![instrument],
                )
            })?
            .map(|r| r.i(0))
            .unwrap_or(0))
    }
}

/// Portable liveness: kill(pid, 0) succeeds, or fails with EPERM (exists, not ours).
fn pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence/permission check.
    let ok = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    ok || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn audit_chain_verifies_and_is_append_only() {
        let s = Store::in_memory().unwrap();
        for i in 0..5 {
            s.audit("system", "test", Some("x"), json!({"i": i}), true)
                .unwrap();
        }
        let r = s.verify_audit().unwrap();
        assert!(r.ok && r.entries == 5, "{r:?}");
        assert!(s
            .raw_batch("UPDATE audit SET actor='evil' WHERE seq=2")
            .is_err());
        assert!(s.raw_batch("DELETE FROM audit WHERE seq=3").is_err());
    }

    #[test]
    fn tampering_is_detected_even_if_triggers_dropped() {
        let s = Store::in_memory().unwrap();
        for i in 0..4 {
            s.audit("system", "test", None, json!({"i": i}), true)
                .unwrap();
        }
        s.raw_batch("DROP TRIGGER audit_no_update; DROP TRIGGER audit_no_delete;")
            .unwrap();
        s.raw_batch("UPDATE audit SET details='{\"i\":99}' WHERE seq=2")
            .unwrap();
        let r = s.verify_audit().unwrap();
        assert!(!r.ok);
        assert_eq!(r.first_bad_seq, Some(2));

        let s2 = Store::in_memory().unwrap();
        for i in 0..4 {
            s2.audit("system", "test", None, json!({"i": i}), true)
                .unwrap();
        }
        s2.raw_batch("DROP TRIGGER audit_no_delete;").unwrap();
        s2.raw_batch("DELETE FROM audit WHERE seq=3").unwrap();
        let r2 = s2.verify_audit().unwrap();
        assert!(!r2.ok && r2.first_bad_seq == Some(4), "{r2:?}");
    }

    fn anchored(dir: &Path) -> (Store, AuditConfig) {
        let cfg = AuditConfig {
            signer: None,
            chain: String::new(),
            key: b"secret-key-outside-db".to_vec(),
            anchor: Some(dir.join("anchors/h.anchor")),
        };
        (Store::open(&dir.join("cmsq.db"), cfg.clone()).unwrap(), cfg)
    }

    /// Reviewer attack: drop triggers, delete the tail. Every remaining link is
    /// valid, so an unanchored chain would verify OK.
    #[test]
    fn attack_tail_truncation_is_detected_by_anchor() {
        let d = tempfile::tempdir().unwrap();
        let (s, _) = anchored(d.path());
        for i in 0..6 {
            s.audit("system", "test", None, json!({"i": i}), true)
                .unwrap();
        }
        assert!(s.verify_audit().unwrap().ok);
        s.raw_batch("DROP TRIGGER audit_no_delete; DELETE FROM audit WHERE seq >= 5;")
            .unwrap();
        let r = s.verify_audit().unwrap();
        assert!(!r.ok, "{r:?}");
        assert!(r.problem.unwrap().contains("truncated"));
    }

    /// Reviewer attack: rewrite an entry and re-chain every hash after it with the
    /// public algorithm. Without the key the MACs cannot be recomputed.
    #[test]
    fn attack_rechain_without_key_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let (s, _) = anchored(d.path());
        for i in 0..4 {
            s.audit(
                "gate",
                "evaluate",
                Some("x"),
                json!({"passed": i % 2 == 0}),
                true,
            )
            .unwrap();
        }
        s.raw_batch("DROP TRIGGER audit_no_update;").unwrap();
        // attacker re-computes the chain with a guessed key
        let rows =
            s.db.run(|e| {
                e.query(
                    "SELECT seq,ts,actor,action,target,details,success FROM audit ORDER BY seq",
                    &[],
                )
            })
            .unwrap();
        let mut prev = GENESIS.to_string();
        for r in rows {
            let (seq, mut details) = (r.i(0), r.s(5));
            if seq == 2 {
                details = json!({"passed": true}).to_string();
            }
            let h = audit::entry_hash(
                b"guess",
                &prev,
                seq,
                &r.s(1),
                &r.s(2),
                &r.s(3),
                r.os(4).as_deref(),
                &details,
                r.b(6),
            );
            s.raw_exec(
                "UPDATE audit SET details=?2, prev_hash=?3, hash=?4 WHERE seq=?1",
                p![seq, details, prev.as_str(), h.as_str()],
            )
            .unwrap();
            prev = h;
        }
        let r = s.verify_audit().unwrap();
        assert!(!r.ok && r.first_bad_seq == Some(1), "{r:?}");
    }

    /// Even an attacker WITH the key who re-chains the DB is caught by the anchor
    /// (the anchored head MAC no longer matches), unless they also rewrite the anchor.
    #[test]
    fn attack_rechain_with_key_is_detected_by_anchor() {
        let d = tempfile::tempdir().unwrap();
        let (s, cfg) = anchored(d.path());
        for i in 0..3 {
            s.audit("gate", "evaluate", None, json!({"i": i}), true)
                .unwrap();
        }
        s.raw_batch("DROP TRIGGER audit_no_update;").unwrap();
        let mut prev = GENESIS.to_string();
        for seq in 1..=3i64 {
            let r =
                s.db.run(|e| e.one("SELECT ts,actor,action FROM audit WHERE seq=?1", p![seq]))
                    .unwrap()
                    .unwrap();
            let details = json!({"i": 100 + seq}).to_string();
            let h = audit::entry_hash(
                &cfg.key,
                &prev,
                seq,
                &r.s(0),
                &r.s(1),
                &r.s(2),
                None,
                &details,
                true,
            );
            s.raw_exec(
                "UPDATE audit SET details=?2, prev_hash=?3, hash=?4 WHERE seq=?1",
                p![seq, details, prev.as_str(), h.as_str()],
            )
            .unwrap();
            prev = h;
        }
        let r = s.verify_audit().unwrap();
        assert!(
            !r.ok && r.problem.as_deref().unwrap().contains("rewritten"),
            "{r:?}"
        );
    }

    /// Replacing the DB with an older backup (rollback) is truncation by another name.
    #[test]
    fn attack_rollback_to_old_copy_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let (s, cfg) = anchored(d.path());
        s.audit("a", "x", None, json!({}), true).unwrap();
        s.raw_batch(&format!(
            "VACUUM INTO '{}'",
            d.path().join("old.db").display()
        ))
        .unwrap();
        s.audit("a", "promote", None, json!({}), true).unwrap();
        drop(s);
        let old = Store::open(&d.path().join("old.db"), cfg).unwrap();
        let r = old.verify_audit().unwrap();
        assert!(!r.ok, "{r:?}");
    }

    #[test]
    fn missing_anchor_with_entries_fails() {
        let d = tempfile::tempdir().unwrap();
        let (s, cfg) = anchored(d.path());
        s.audit("a", "x", None, json!({}), true).unwrap();
        std::fs::remove_file(cfg.anchor.unwrap()).unwrap();
        assert!(!s.verify_audit().unwrap().ok);
    }

    #[test]
    fn signals_are_observe_only_by_schema() {
        let s = Store::in_memory().unwrap();
        let e = s.raw_batch("INSERT INTO signals(strategy_id,job_id,symbol,date,target_position,observe_only,created_at) VALUES('a','b','c','d',1,0,'t')");
        assert!(e.is_err());
    }
}
