use crate::audit::{self, AuditConfig, VerifyReport, GENESIS};
use crate::models::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;

const SCHEMA: &str = r#"
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
CREATE TRIGGER IF NOT EXISTS audit_no_update BEFORE UPDATE ON audit
  BEGIN SELECT RAISE(ABORT, 'audit log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_no_delete BEFORE DELETE ON audit
  BEGIN SELECT RAISE(ABORT, 'audit log is append-only'); END;
"#;

pub struct Store {
    pub conn: Connection,
    cfg: AuditConfig,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TrialRow {
    pub strategy_id: String,
    pub idea_key: String,
    pub params_hash: String,
    pub symbol: String,
    pub sharpe_daily: f64,
    pub positions: Vec<f64>,
    /// run was caught by the look-ahead probe: counts toward N, excluded from the SR variance
    pub lookahead: bool,
}

fn ts(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)).unwrap_or_default()
}

impl Store {
    pub fn open(path: &Path, cfg: AuditConfig) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        // forward migration for P0 databases (columns added after the first release)
        for (col, ddl) in [("owner_pid", "INTEGER"), ("runner_identity", "TEXT"), ("kind", "TEXT NOT NULL DEFAULT 'dev'")] {
            let has: bool = conn.prepare("SELECT 1 FROM pragma_table_info('jobs') WHERE name=?1")?.exists([col])?;
            if !has {
                conn.execute_batch(&format!("ALTER TABLE jobs ADD COLUMN {col} {ddl}"))?;
            }
        }
        for (col, ddl) in [("idea_key", "TEXT"), ("symbol", "TEXT"), ("code_sha", "TEXT")] {
            let has: bool = conn.prepare("SELECT 1 FROM pragma_table_info('vault_touches') WHERE name=?1")?.exists([col])?;
            if !has {
                conn.execute_batch(&format!("ALTER TABLE vault_touches ADD COLUMN {col} {ddl}"))?;
            }
        }
        let has: bool = conn.prepare("SELECT 1 FROM pragma_table_info('trials') WHERE name='lookahead'")?.exists([])?;
        if !has {
            conn.execute_batch("ALTER TABLE trials ADD COLUMN lookahead INTEGER NOT NULL DEFAULT 0")?;
        }
        Ok(Self { conn, cfg })
    }
    pub fn open_readonly(path: &Path, cfg: AuditConfig) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn, cfg })
    }
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, cfg: AuditConfig::insecure_test() })
    }

    // ---------- audit ----------
    pub fn audit(&self, actor: &str, action: &str, target: Option<&str>, details: serde_json::Value, success: bool) -> Result<AuditEntry> {
        let tx = self.conn.unchecked_transaction()?;
        let (prev_seq, prev_hash): (i64, String) = tx
            .query_row("SELECT seq, hash FROM audit ORDER BY seq DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .unwrap_or((0, GENESIS.to_string()));
        let seq = prev_seq + 1;
        let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let d = serde_json::to_string(&details)?;
        let hash = audit::entry_hash(&self.cfg.key, &prev_hash, seq, &ts, actor, action, target, &d, success);
        tx.execute(
            "INSERT INTO audit(seq,ts,actor,action,target,details,success,prev_hash,hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![seq, ts, actor, action, target, d, success as i64, prev_hash, hash],
        )?;
        tx.commit()?;
        if let Some(a) = &self.cfg.anchor {
            audit::append_anchor(a, seq, &hash, &ts).with_context(|| format!("write audit anchor {}", a.display()))?;
        }
        Ok(AuditEntry { seq, ts, actor: actor.into(), action: action.into(), target: target.map(Into::into), details, success, prev_hash, hash })
    }

    fn audit_row(r: &Row) -> rusqlite::Result<AuditEntry> {
        let d: String = r.get(5)?;
        Ok(AuditEntry {
            seq: r.get(0)?, ts: r.get(1)?, actor: r.get(2)?, action: r.get(3)?, target: r.get(4)?,
            details: serde_json::from_str(&d).unwrap_or(serde_json::Value::String(d)),
            success: r.get::<_, i64>(6)? != 0, prev_hash: r.get(7)?, hash: r.get(8)?,
        })
    }

    pub fn audit_entries(&self, target: Option<&str>, limit: i64) -> Result<Vec<AuditEntry>> {
        let sql = "SELECT seq,ts,actor,action,target,details,success,prev_hash,hash FROM audit WHERE (?1 IS NULL OR target=?1) ORDER BY seq DESC LIMIT ?2";
        let mut st = self.conn.prepare(sql)?;
        let v = st.query_map(params![target, limit], Self::audit_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    /// Recompute the whole chain. Detects edits, deletions (seq gaps) and re-ordering.
    pub fn verify_audit(&self) -> Result<VerifyReport> {
        let mut st = self.conn.prepare("SELECT seq,ts,actor,action,target,details,success,prev_hash,hash FROM audit ORDER BY seq ASC")?;
        let mut rows = st.query([])?;
        let mut prev = GENESIS.to_string();
        let mut expect_seq = 1;
        let mut n = 0;
        while let Some(r) = rows.next()? {
            let (seq, ts, actor, action, target, details, success, prev_hash, hash): (i64, String, String, String, Option<String>, String, i64, String, String) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?);
            n += 1;
            let bad = |p: String| VerifyReport { entries: n, ok: false, first_bad_seq: Some(seq), problem: Some(p), head_hash: hash.clone(), keyed: true,
                anchor: self.cfg.anchor.as_ref().map(|a| a.display().to_string()), anchored_seq: None };
            if seq != expect_seq {
                return Ok(bad(format!("sequence gap: expected {expect_seq}, found {seq} (entry deleted?)")));
            }
            if prev_hash != prev {
                return Ok(bad("prev_hash does not link to previous entry".into()));
            }
            let calc = audit::entry_hash(&self.cfg.key, &prev_hash, seq, &ts, &actor, &action, target.as_deref(), &details, success != 0);
            if calc != hash {
                if seq == 1 {
                    let why = "this workspace's audit chain was not signed with the current key: it was created by an older cmsq (unkeyed sha256 chain) or with a different audit.key / CMSQ_SECRETS_DIR. Use a fresh CMSQ_HOME, or point CMSQ_SECRETS_DIR at the key that created it";
                    return Ok(bad(format!("HMAC mismatch at the first entry — {why}")));
                }
                return Ok(bad("HMAC mismatch (entry modified or chain re-computed without the key)".into()));
            }
            prev = hash;
            expect_seq += 1;
        }
        let head_seq = expect_seq - 1;
        let mut rep = VerifyReport { entries: n, ok: true, first_bad_seq: None, problem: None, head_hash: prev.clone(), keyed: true,
            anchor: self.cfg.anchor.as_ref().map(|a| a.display().to_string()), anchored_seq: None };
        if let Some(a) = &self.cfg.anchor {
            match audit::read_anchor(a)? {
                None if head_seq > 0 => {
                    rep.ok = false;
                    rep.problem = Some(format!("anchor file {} is missing but the log has {head_seq} entries", a.display()));
                }
                None => {}
                Some((aseq, ahash)) => {
                    rep.anchored_seq = Some(aseq);
                    if aseq > head_seq {
                        rep.ok = false;
                        rep.first_bad_seq = Some(head_seq + 1);
                        rep.problem = Some(format!("log truncated: anchor says head is seq {aseq}, database ends at {head_seq}"));
                    } else {
                        let h: Option<String> = self.conn.query_row("SELECT hash FROM audit WHERE seq=?1", [aseq], |r| r.get(0)).optional()?;
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
        Ok(rep)
    }

    // ---------- strategies ----------
    pub fn insert_strategy(&self, s: &Strategy) -> Result<()> {
        self.conn.execute(
            "INSERT INTO strategies(id,title,source_site,source_url,author,scraped_at,ingested_at,raw_path,content_sha256,sample,status,attempts,last_reason,extracted_json,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![s.id, s.title, s.source_site, s.source_url, s.author, s.scraped_at, s.ingested_at.to_rfc3339(), s.raw_path,
                s.content_sha256, s.sample as i64, s.status.as_str(), s.attempts, s.last_reason,
                s.extracted.as_ref().map(|e| serde_json::to_string(e).unwrap()), s.updated_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn find_by_sha(&self, sha: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT id FROM strategies WHERE content_sha256=?1", [sha], |r| r.get(0)).optional()?)
    }

    fn strategy_row(r: &Row) -> rusqlite::Result<Strategy> {
        let ex: Option<String> = r.get(13)?;
        Ok(Strategy {
            id: r.get(0)?, title: r.get(1)?, source_site: r.get(2)?, source_url: r.get(3)?, author: r.get(4)?,
            scraped_at: r.get(5)?, ingested_at: ts(r.get(6)?), raw_path: r.get(7)?, content_sha256: r.get(8)?,
            sample: r.get::<_, i64>(9)? != 0,
            status: StrategyStatus::parse(&r.get::<_, String>(10)?).unwrap_or(StrategyStatus::Failed),
            attempts: r.get(11)?, last_reason: r.get(12)?,
            extracted: ex.and_then(|e| serde_json::from_str(&e).ok()), updated_at: ts(r.get(14)?),
        })
    }
    const SCOLS: &'static str = "id,title,source_site,source_url,author,scraped_at,ingested_at,raw_path,content_sha256,sample,status,attempts,last_reason,extracted_json,updated_at";

    pub fn get_strategy(&self, id: &str) -> Result<Strategy> {
        let sql = format!("SELECT {} FROM strategies WHERE id=?1 OR id LIKE ?1 || '%' ORDER BY id LIMIT 2", Self::SCOLS);
        let mut st = self.conn.prepare(&sql)?;
        let v = st.query_map([id], Self::strategy_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        match v.len() {
            0 => anyhow::bail!("strategy not found: {id}"),
            1 => Ok(v.into_iter().next().unwrap()),
            _ => anyhow::bail!("ambiguous strategy id prefix: {id}"),
        }
    }

    pub fn list_strategies(&self, status: Option<StrategyStatus>) -> Result<Vec<Strategy>> {
        let sql = format!("SELECT {} FROM strategies WHERE (?1 IS NULL OR status=?1) ORDER BY ingested_at, id", Self::SCOLS);
        let mut st = self.conn.prepare(&sql)?;
        let v = st.query_map([status.map(|s| s.as_str())], Self::strategy_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn set_status(&self, id: &str, to: StrategyStatus, reason: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE strategies SET status=?2, last_reason=?3, updated_at=?4 WHERE id=?1",
            params![id, to.as_str(), reason, Utc::now().to_rfc3339()])?;
        Ok(())
    }
    pub fn set_extracted(&self, id: &str, e: &ExtractedStrategy) -> Result<()> {
        self.conn.execute("UPDATE strategies SET extracted_json=?2, updated_at=?3 WHERE id=?1",
            params![id, serde_json::to_string(e)?, Utc::now().to_rfc3339()])?;
        Ok(())
    }
    pub fn bump_attempts(&self, id: &str) -> Result<i64> {
        self.conn.execute("UPDATE strategies SET attempts=attempts+1 WHERE id=?1", [id])?;
        Ok(self.conn.query_row("SELECT attempts FROM strategies WHERE id=?1", [id], |r| r.get(0))?)
    }

    // ---------- jobs ----------
    pub fn insert_job(&self, j: &JobRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO jobs(id,strategy_id,backend,image,symbol,data_source,status,started_at,job_dir,owner_pid,runner_identity,kind) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![j.id, j.strategy_id, j.backend, j.image, j.symbol, j.data_source, j.status, j.started_at.to_rfc3339(), j.job_dir,
                std::process::id() as i64, j.runner_identity, j.kind],
        )?;
        Ok(())
    }
    pub fn finish_job(&self, id: &str, status: &str, exit_code: Option<i64>, duration_ms: i64, error: Option<&str>, result: Option<&serde_json::Value>) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET status=?2, exit_code=?3, finished_at=?4, duration_ms=?5, error=?6, result_json=?7 WHERE id=?1",
            params![id, status, exit_code, Utc::now().to_rfc3339(), duration_ms, error, result.map(|r| r.to_string())],
        )?;
        Ok(())
    }
    fn job_row(r: &Row) -> rusqlite::Result<JobRecord> {
        let res: Option<String> = r.get(13)?;
        Ok(JobRecord {
            id: r.get(0)?, strategy_id: r.get(1)?, backend: r.get(2)?, image: r.get(3)?, symbol: r.get(4)?,
            data_source: r.get(5)?, status: r.get(6)?, exit_code: r.get(7)?, started_at: ts(r.get(8)?),
            finished_at: r.get::<_, Option<String>>(9)?.map(ts), duration_ms: r.get(10)?, job_dir: r.get(11)?,
            error: r.get(12)?, result: res.and_then(|s| serde_json::from_str(&s).ok()),
            owner_pid: r.get(14)?, runner_identity: r.get(15)?, kind: r.get(16)?,
        })
    }
    pub fn list_jobs(&self, strategy_id: Option<&str>) -> Result<Vec<JobRecord>> {
        let mut st = self.conn.prepare("SELECT id,strategy_id,backend,image,symbol,data_source,status,exit_code,started_at,finished_at,duration_ms,job_dir,error,result_json,owner_pid,runner_identity,kind FROM jobs WHERE (?1 IS NULL OR strategy_id=?1) ORDER BY started_at DESC")?;
        let v = st.query_map([strategy_id], Self::job_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    /// Jobs left in `running` by a process that is gone (crash / kill -9).
    pub fn stale_running_jobs(&self) -> Result<Vec<JobRecord>> {
        Ok(self.list_jobs(None)?.into_iter().filter(|j| j.status == "running" && !j.owner_pid.is_some_and(pid_alive)).collect())
    }

    // ---------- trials (library-wide multiple-testing ledger) ----------
    #[allow(clippy::too_many_arguments)]
    pub fn insert_trial(&self, sid: &str, job: &str, idea_key: &str, params_hash: &str, symbol: &str, dataset_sha: &str, bars: usize, sr: f64, positions: &[f64], lookahead: bool) -> Result<()> {
        self.conn.execute("INSERT INTO trials(strategy_id,job_id,idea_key,params_hash,symbol,dataset_sha,bars,sharpe_daily,positions_json,created_at,lookahead) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![sid, job, idea_key, params_hash, symbol, dataset_sha, bars as i64, sr, serde_json::to_string(positions)?, Utc::now().to_rfc3339(), lookahead as i64])?;
        Ok(())
    }
    pub fn trials(&self) -> Result<Vec<TrialRow>> {
        let mut st = self.conn.prepare("SELECT strategy_id,idea_key,params_hash,symbol,sharpe_daily,positions_json,lookahead FROM trials ORDER BY id")?;
        let v = st.query_map([], |r| Ok(TrialRow { strategy_id: r.get(0)?, idea_key: r.get(1)?, params_hash: r.get(2)?, symbol: r.get(3)?, sharpe_daily: r.get(4)?,
            positions: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(), lookahead: r.get::<_, i64>(6)? != 0 }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }
    pub fn trial_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM trials", [], |r| r.get(0))?)
    }

    // ---------- vault (final holdout, one touch per strategy) ----------
    pub fn vault_touch(&self, sid: &str) -> Result<Option<(String, bool)>> {
        Ok(self.conn.query_row("SELECT job_id, passed FROM vault_touches WHERE strategy_id=?1", [sid], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0))).optional()?)
    }
    pub fn record_vault_touch(&self, sid: &str, job: &str, passed: bool) -> Result<()> {
        self.record_vault_touch_keyed(sid, job, passed, None, None, None)
    }
    pub fn record_vault_touch_keyed(&self, sid: &str, job: &str, passed: bool, idea_key: Option<&str>, symbol: Option<&str>, code_sha: Option<&str>) -> Result<()> {
        self.conn.execute("INSERT INTO vault_touches(strategy_id,job_id,passed,created_at,idea_key,symbol,code_sha) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![sid, job, passed as i64, Utc::now().to_rfc3339(), idea_key, symbol, code_sha])?;
        Ok(())
    }
    /// (strategy_id, idea_key, symbol, code_sha) of every vault run so far.
    #[allow(clippy::type_complexity)]
    pub fn vault_touches(&self) -> Result<Vec<(String, Option<String>, Option<String>, Option<String>)>> {
        let mut st = self.conn.prepare("SELECT strategy_id, idea_key, symbol, code_sha FROM vault_touches ORDER BY created_at")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    /// Latest successful `market_data_fetch` audit entry for `symbol`, with its
    /// own HMAC re-verified (an edited row is refused).
    #[allow(clippy::type_complexity)]
    pub fn latest_fetch_attestation(&self, symbol: &str) -> Result<Option<AuditEntry>> {
        let row: Option<(i64, String, String, String, Option<String>, String, i64, String, String)> = self.conn.query_row(
            "SELECT seq,ts,actor,action,target,details,success,prev_hash,hash FROM audit WHERE action='market_data_fetch' AND target=?1 AND success=1 ORDER BY seq DESC LIMIT 1",
            [symbol], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))).optional()?;
        let Some((seq, ts, actor, action, target, d, ok, prev, hash)) = row else { return Ok(None) };
        let expect = audit::entry_hash(&self.cfg.key, &prev, seq, &ts, &actor, &action, target.as_deref(), &d, ok != 0);
        anyhow::ensure!(expect == hash, crate::error::CoreError::AuditIntegrity(format!("data attestation entry seq {seq} fails its HMAC")));
        Ok(Some(AuditEntry { seq, ts, actor, action, target, details: serde_json::from_str(&d)?, success: ok != 0, prev_hash: prev, hash }))
    }

    // ---------- evaluations / signals ----------
    pub fn insert_evaluation(&self, e: &Evaluation) -> Result<()> {
        self.conn.execute("INSERT INTO evaluations(id,strategy_id,job_id,passed,reasons_json,checks_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![e.id, e.strategy_id, e.job_id, e.passed as i64, serde_json::to_string(&e.reasons)?, e.checks.to_string(), e.created_at.to_rfc3339()])?;
        Ok(())
    }
    pub fn latest_evaluation(&self, sid: &str) -> Result<Option<Evaluation>> {
        Ok(self.conn.query_row("SELECT id,strategy_id,job_id,passed,reasons_json,checks_json,created_at FROM evaluations WHERE strategy_id=?1 ORDER BY created_at DESC, rowid DESC LIMIT 1", [sid], |r| {
            Ok(Evaluation { id: r.get(0)?, strategy_id: r.get(1)?, job_id: r.get(2)?, passed: r.get::<_, i64>(3)? != 0,
                reasons: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                checks: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(), created_at: ts(r.get(6)?) })
        }).optional()?)
    }
    pub fn insert_signal(&self, s: &Signal) -> Result<()> {
        self.conn.execute("INSERT INTO signals(strategy_id,job_id,symbol,date,target_position,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![s.strategy_id, s.job_id, s.symbol, s.date, s.target_position, s.created_at.to_rfc3339()])?;
        Ok(())
    }
    pub fn list_signals(&self) -> Result<Vec<(Signal, String, String)>> {
        let mut st = self.conn.prepare("SELECT g.strategy_id,g.job_id,g.symbol,g.date,g.target_position,g.created_at,s.title,s.status FROM signals g JOIN strategies s ON s.id=g.strategy_id ORDER BY g.id DESC")?;
        let v = st.query_map([], |r| Ok((Signal { strategy_id: r.get(0)?, job_id: r.get(1)?, symbol: r.get(2)?, date: r.get(3)?, target_position: r.get(4)?, created_at: ts(r.get(5)?) }, r.get(6)?, r.get(7)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn counts(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare("SELECT status, COUNT(*) FROM strategies GROUP BY status")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }
    pub fn total_attempts(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(SUM(attempts),0) FROM strategies", [], |r| r.get(0))?)
    }
}

fn pid_alive(pid: i64) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn audit_chain_verifies_and_is_append_only() {
        let s = Store::in_memory().unwrap();
        for i in 0..5 {
            s.audit("system", "test", Some("x"), json!({"i": i}), true).unwrap();
        }
        let r = s.verify_audit().unwrap();
        assert!(r.ok && r.entries == 5, "{r:?}");
        assert!(s.conn.execute("UPDATE audit SET actor='evil' WHERE seq=2", []).is_err());
        assert!(s.conn.execute("DELETE FROM audit WHERE seq=3", []).is_err());
    }

    #[test]
    fn tampering_is_detected_even_if_triggers_dropped() {
        let s = Store::in_memory().unwrap();
        for i in 0..4 {
            s.audit("system", "test", None, json!({"i": i}), true).unwrap();
        }
        s.conn.execute_batch("DROP TRIGGER audit_no_update; DROP TRIGGER audit_no_delete;").unwrap();
        s.conn.execute("UPDATE audit SET details='{\"i\":99}' WHERE seq=2", []).unwrap();
        let r = s.verify_audit().unwrap();
        assert!(!r.ok);
        assert_eq!(r.first_bad_seq, Some(2));

        let s2 = Store::in_memory().unwrap();
        for i in 0..4 {
            s2.audit("system", "test", None, json!({"i": i}), true).unwrap();
        }
        s2.conn.execute_batch("DROP TRIGGER audit_no_delete;").unwrap();
        s2.conn.execute("DELETE FROM audit WHERE seq=3", []).unwrap();
        let r2 = s2.verify_audit().unwrap();
        assert!(!r2.ok && r2.first_bad_seq == Some(4), "{r2:?}");
    }

    fn anchored(dir: &Path) -> (Store, AuditConfig) {
        let cfg = AuditConfig { key: b"secret-key-outside-db".to_vec(), anchor: Some(dir.join("anchors/h.anchor")) };
        (Store::open(&dir.join("cmsq.db"), cfg.clone()).unwrap(), cfg)
    }

    /// Reviewer attack: drop triggers, delete the tail. Every remaining link is
    /// valid, so an unanchored chain would verify OK.
    #[test]
    fn attack_tail_truncation_is_detected_by_anchor() {
        let d = tempfile::tempdir().unwrap();
        let (s, _) = anchored(d.path());
        for i in 0..6 {
            s.audit("system", "test", None, json!({"i": i}), true).unwrap();
        }
        assert!(s.verify_audit().unwrap().ok);
        s.conn.execute_batch("DROP TRIGGER audit_no_delete; DELETE FROM audit WHERE seq >= 5;").unwrap();
        let r = s.verify_audit().unwrap();
        assert!(!r.ok, "{r:?}");
        assert!(r.problem.unwrap().contains("truncated"));
    }

    /// Reviewer attack: rewrite an entry and re-chain every hash after it with the
    /// public algorithm. Without the key the MACs cannot be recomputed.
    #[test]
    #[allow(clippy::type_complexity)]
    fn attack_rechain_without_key_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let (s, _) = anchored(d.path());
        for i in 0..4 {
            s.audit("gate", "evaluate", Some("x"), json!({"passed": i % 2 == 0}), true).unwrap();
        }
        s.conn.execute_batch("DROP TRIGGER audit_no_update;").unwrap();
        // attacker re-computes the chain with a guessed key
        let rows: Vec<(i64, String, String, String, Option<String>, String, i64)> = s.conn.prepare("SELECT seq,ts,actor,action,target,details,success FROM audit ORDER BY seq").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).unwrap().map(|x| x.unwrap()).collect();
        let mut prev = GENESIS.to_string();
        for (seq, ts, actor, action, target, mut details, success) in rows {
            if seq == 2 {
                details = json!({"passed": true}).to_string();
            }
            let h = audit::entry_hash(b"guess", &prev, seq, &ts, &actor, &action, target.as_deref(), &details, success != 0);
            s.conn.execute("UPDATE audit SET details=?2, prev_hash=?3, hash=?4 WHERE seq=?1", params![seq, details, prev, h]).unwrap();
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
            s.audit("gate", "evaluate", None, json!({"i": i}), true).unwrap();
        }
        s.conn.execute_batch("DROP TRIGGER audit_no_update;").unwrap();
        let mut prev = GENESIS.to_string();
        for seq in 1..=3i64 {
            let (ts, actor, action): (String, String, String) = s.conn.query_row("SELECT ts,actor,action FROM audit WHERE seq=?1", [seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
            let details = json!({"i": 100 + seq}).to_string();
            let h = audit::entry_hash(&cfg.key, &prev, seq, &ts, &actor, &action, None, &details, true);
            s.conn.execute("UPDATE audit SET details=?2, prev_hash=?3, hash=?4 WHERE seq=?1", params![seq, details, prev, h]).unwrap();
            prev = h;
        }
        let r = s.verify_audit().unwrap();
        assert!(!r.ok && r.problem.as_deref().unwrap().contains("rewritten"), "{r:?}");
    }

    /// Replacing the DB with an older backup (rollback) is truncation by another name.
    #[test]
    fn attack_rollback_to_old_copy_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let (s, cfg) = anchored(d.path());
        s.audit("a", "x", None, json!({}), true).unwrap();
        s.conn.execute_batch(&format!("VACUUM INTO '{}'", d.path().join("old.db").display())).unwrap();
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
        let e = s.conn.execute("INSERT INTO signals(strategy_id,job_id,symbol,date,target_position,observe_only,created_at) VALUES('a','b','c','d',1,0,'t')", []);
        assert!(e.is_err());
    }
}
