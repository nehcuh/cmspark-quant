//! Minimal dialect-neutral SQL layer: one SQL text (SQLite `?N` placeholders,
//! rewritten to `$N` for PostgreSQL), dynamically typed values and rows, and a
//! transaction primitive with an exclusive *named lock* (PostgreSQL
//! `pg_advisory_xact_lock`, SQLite `BEGIN IMMEDIATE`) so read-check-write
//! sequences (audit head, trial ledger, vault claim) are linearizable across
//! threads, processes and hosts.
use anyhow::{Context, Result};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub enum V {
    /// NULL bound as text
    Null,
    /// NULL bound as bigint
    NullI,
    /// NULL bound as double
    NullF,
    I(i64),
    F(f64),
    T(String),
}

impl From<&str> for V {
    fn from(s: &str) -> Self {
        V::T(s.into())
    }
}
impl From<String> for V {
    fn from(s: String) -> Self {
        V::T(s)
    }
}
impl From<&String> for V {
    fn from(s: &String) -> Self {
        V::T(s.clone())
    }
}
impl From<i64> for V {
    fn from(i: i64) -> Self {
        V::I(i)
    }
}
impl From<usize> for V {
    fn from(i: usize) -> Self {
        V::I(i as i64)
    }
}
impl From<f64> for V {
    fn from(f: f64) -> Self {
        V::F(f)
    }
}
impl From<bool> for V {
    fn from(b: bool) -> Self {
        V::I(b as i64)
    }
}
impl From<Option<&str>> for V {
    fn from(s: Option<&str>) -> Self {
        s.map(V::from).unwrap_or(V::Null)
    }
}
impl From<Option<String>> for V {
    fn from(s: Option<String>) -> Self {
        s.map(V::T).unwrap_or(V::Null)
    }
}
impl From<&Option<String>> for V {
    fn from(s: &Option<String>) -> Self {
        s.clone().map(V::T).unwrap_or(V::Null)
    }
}
impl From<Option<i64>> for V {
    fn from(i: Option<i64>) -> Self {
        i.map(V::I).unwrap_or(V::NullI)
    }
}
impl From<Option<f64>> for V {
    fn from(f: Option<f64>) -> Self {
        f.map(V::F).unwrap_or(V::NullF)
    }
}

#[macro_export]
macro_rules! p {
    ($($x:expr),* $(,)?) => { &[$($crate::store::sql::V::from($x)),*] as &[$crate::store::sql::V] };
}

#[derive(Debug, Clone)]
pub struct R(pub Vec<V>);

impl R {
    pub fn s(&self, i: usize) -> String {
        self.os(i).unwrap_or_default()
    }
    pub fn os(&self, i: usize) -> Option<String> {
        match &self.0[i] {
            V::T(s) => Some(s.clone()),
            V::I(x) => Some(x.to_string()),
            V::F(x) => Some(x.to_string()),
            _ => None,
        }
    }
    pub fn oi(&self, i: usize) -> Option<i64> {
        match &self.0[i] {
            V::I(x) => Some(*x),
            V::F(x) => Some(*x as i64),
            V::T(s) => s.parse().ok(),
            _ => None,
        }
    }
    pub fn i(&self, i: usize) -> i64 {
        self.oi(i).unwrap_or(0)
    }
    pub fn f(&self, i: usize) -> f64 {
        match &self.0[i] {
            V::F(x) => *x,
            V::I(x) => *x as f64,
            V::T(s) => s.parse().unwrap_or(0.0),
            _ => 0.0,
        }
    }
    pub fn b(&self, i: usize) -> bool {
        self.i(i) != 0
    }
}

/// Statement executor inside (or outside) a transaction.
pub trait Exec {
    fn query(&mut self, sql: &str, p: &[V]) -> Result<Vec<R>>;
    fn exec(&mut self, sql: &str, p: &[V]) -> Result<u64>;
    fn one(&mut self, sql: &str, p: &[V]) -> Result<Option<R>> {
        Ok(self.query(sql, p)?.into_iter().next())
    }
}

/// Exclusive named locks (advisory-lock keys).
#[derive(Debug, Clone, Copy)]
pub enum Lock {
    Audit,
    Ledger,
    Vault,
}

impl Lock {
    pub fn key(self) -> i64 {
        match self {
            Lock::Audit => 0x434D_5351_0001,
            Lock::Ledger => 0x434D_5351_0002,
            Lock::Vault => 0x434D_5351_0003,
        }
    }
}

// ---------------- SQLite ----------------
pub struct SqliteExec<'a>(pub &'a rusqlite::Connection);

fn to_sql(v: &V) -> rusqlite::types::Value {
    use rusqlite::types::Value as Q;
    match v {
        V::Null | V::NullI | V::NullF => Q::Null,
        V::I(i) => Q::Integer(*i),
        V::F(f) => Q::Real(*f),
        V::T(s) => Q::Text(s.clone()),
    }
}

impl Exec for SqliteExec<'_> {
    fn query(&mut self, sql: &str, p: &[V]) -> Result<Vec<R>> {
        let mut st = self
            .0
            .prepare_cached(sql)
            .with_context(|| format!("prepare: {sql}"))?;
        let n = st.column_count();
        let params: Vec<rusqlite::types::Value> = p.iter().map(to_sql).collect();
        let mut rows = st.query(rusqlite::params_from_iter(params.iter()))?;
        let mut out = vec![];
        while let Some(r) = rows.next()? {
            let mut v = Vec::with_capacity(n);
            for i in 0..n {
                use rusqlite::types::ValueRef as Q;
                v.push(match r.get_ref(i)? {
                    Q::Null => V::Null,
                    Q::Integer(x) => V::I(x),
                    Q::Real(x) => V::F(x),
                    Q::Text(t) => V::T(String::from_utf8_lossy(t).into_owned()),
                    Q::Blob(b) => V::T(hex::encode(b)),
                });
            }
            out.push(R(v));
        }
        Ok(out)
    }
    fn exec(&mut self, sql: &str, p: &[V]) -> Result<u64> {
        let mut st = self
            .0
            .prepare_cached(sql)
            .with_context(|| format!("prepare: {sql}"))?;
        let params: Vec<rusqlite::types::Value> = p.iter().map(to_sql).collect();
        Ok(st.execute(rusqlite::params_from_iter(params.iter()))? as u64)
    }
}

// ---------------- PostgreSQL (sqlx pool behind a sync facade) ----------------
pub struct Pg {
    rt: Option<tokio::runtime::Runtime>,
    pub pool: sqlx::PgPool,
}

impl Drop for Pg {
    fn drop(&mut self) {
        // dropping a runtime inside an async context panics; shut it down in the background
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// `?12` → `$12` (outside of string literals).
pub fn pg_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut in_str = false;
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if c == '\'' {
            in_str = !in_str;
        }
        if c == '?' && !in_str && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            out.push('$');
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

impl Pg {
    pub fn connect(url: &str, schema: Option<&str>, max_conns: u32) -> Result<Self> {
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
        use std::str::FromStr;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("cmsq-pg")
            .enable_all()
            .build()?;
        let mut opts = PgConnectOptions::from_str(url).context("bad database url")?;
        if let Some(s) = schema {
            opts = opts.options([("search_path", s)]);
        }
        let pool = std::thread::scope(|sc| {
            sc.spawn(|| {
                rt.block_on(
                    PgPoolOptions::new()
                        .max_connections(max_conns)
                        .acquire_timeout(std::time::Duration::from_secs(15))
                        .connect_with(opts),
                )
            })
            .join()
            .expect("pg connect thread")
        })
        .context("connect to PostgreSQL (is `scripts/db_up.sh` running?)")?;
        Ok(Self { rt: Some(rt), pool })
    }

    /// Return a pooled connection from inside the runtime (its Drop needs a tokio context).
    pub fn release(&self, conn: sqlx::pool::PoolConnection<sqlx::Postgres>) {
        self.block(async move { drop(conn) });
    }

    /// Run a future on the store's own runtime from sync code, also when the
    /// caller is itself a tokio worker (block_in_place keeps the worker pool alive).
    pub fn block<F>(&self, f: F) -> F::Output
    where
        F: std::future::Future + Send,
        F::Output: Send,
    {
        let rt = self.rt.as_ref().expect("runtime alive");
        let run = || {
            std::thread::scope(|s| {
                s.spawn(|| rt.block_on(f))
                    .join()
                    .expect("pg worker thread panicked")
            })
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(run)
            }
            _ => run(),
        }
    }
}

pub struct PgExec<'a> {
    pub pg: &'a Pg,
    pub conn: &'a mut sqlx::pool::PoolConnection<sqlx::Postgres>,
}

fn decode_pg(row: &sqlx::postgres::PgRow) -> Result<R> {
    use sqlx::{Column, Row, TypeInfo};
    let mut v = Vec::with_capacity(row.columns().len());
    for (i, c) in row.columns().iter().enumerate() {
        let t = c.type_info().name().to_uppercase();
        v.push(match t.as_str() {
            "INT8" | "BIGINT" => row
                .try_get::<Option<i64>, _>(i)?
                .map(V::I)
                .unwrap_or(V::Null),
            "INT4" | "INT" | "INTEGER" => row
                .try_get::<Option<i32>, _>(i)?
                .map(|x| V::I(x as i64))
                .unwrap_or(V::Null),
            "INT2" | "SMALLINT" => row
                .try_get::<Option<i16>, _>(i)?
                .map(|x| V::I(x as i64))
                .unwrap_or(V::Null),
            "BOOL" | "BOOLEAN" => row
                .try_get::<Option<bool>, _>(i)?
                .map(|x| V::I(x as i64))
                .unwrap_or(V::Null),
            "FLOAT8" | "DOUBLE PRECISION" => row
                .try_get::<Option<f64>, _>(i)?
                .map(V::F)
                .unwrap_or(V::Null),
            "FLOAT4" | "REAL" => row
                .try_get::<Option<f32>, _>(i)?
                .map(|x| V::F(x as f64))
                .unwrap_or(V::Null),
            "DATE" => row
                .try_get::<Option<chrono::NaiveDate>, _>(i)?
                .map(|d| V::T(d.format("%Y-%m-%d").to_string()))
                .unwrap_or(V::Null),
            "VOID" => V::Null,
            _ => row
                .try_get::<Option<String>, _>(i)
                .with_context(|| format!("column {i} of type {t}"))?
                .map(V::T)
                .unwrap_or(V::Null),
        });
    }
    Ok(R(v))
}

fn bind<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    p: &'q [V],
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for v in p {
        q = match v {
            V::Null => q.bind(None::<String>),
            V::NullI => q.bind(None::<i64>),
            V::NullF => q.bind(None::<f64>),
            V::I(i) => q.bind(*i),
            V::F(f) => q.bind(*f),
            V::T(s) => q.bind(s.as_str()),
        };
    }
    q
}

impl Exec for PgExec<'_> {
    fn query(&mut self, sql: &str, p: &[V]) -> Result<Vec<R>> {
        let sql = pg_sql(sql);
        let conn = &mut *self.conn;
        self.pg.block(async move {
            let rows = bind(sqlx::query(&sql), p)
                .fetch_all(&mut **conn)
                .await
                .with_context(|| format!("pg: {sql}"))?;
            rows.iter().map(decode_pg).collect::<Result<Vec<_>>>()
        })
    }
    fn exec(&mut self, sql: &str, p: &[V]) -> Result<u64> {
        let sql = pg_sql(sql);
        let conn = &mut *self.conn;
        self.pg.block(async move {
            Ok(bind(sqlx::query(&sql), p)
                .execute(&mut **conn)
                .await
                .with_context(|| format!("pg: {sql}"))?
                .rows_affected())
        })
    }
}

// ---------------- backend ----------------
pub enum Backend {
    Sqlite(Mutex<rusqlite::Connection>),
    Postgres(Pg),
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Sqlite(_) => "sqlite",
            Backend::Postgres(_) => "postgres",
        }
    }

    /// Autocommit statement(s) outside an explicit transaction.
    pub fn run<T>(&self, f: impl FnOnce(&mut dyn Exec) -> Result<T>) -> Result<T> {
        match self {
            Backend::Sqlite(c) => {
                let g = c.lock().unwrap_or_else(|e| e.into_inner());
                f(&mut SqliteExec(&g))
            }
            Backend::Postgres(pg) => {
                let mut conn = pg.block(pg.pool.acquire())?;
                let r = f(&mut PgExec {
                    pg,
                    conn: &mut conn,
                });
                pg.release(conn);
                r
            }
        }
    }

    /// One transaction; with `lock`, holds that exclusive named lock until commit.
    pub fn tx<T>(
        &self,
        lock: Option<Lock>,
        f: impl FnOnce(&mut dyn Exec) -> Result<T>,
    ) -> Result<T> {
        match self {
            Backend::Sqlite(c) => {
                let g = c.lock().unwrap_or_else(|e| e.into_inner());
                // IMMEDIATE takes the database write lock up front: every write tx is serialized
                g.execute_batch("BEGIN IMMEDIATE")?;
                match f(&mut SqliteExec(&g)) {
                    Ok(v) => {
                        g.execute_batch("COMMIT")?;
                        Ok(v)
                    }
                    Err(e) => {
                        let _ = g.execute_batch("ROLLBACK");
                        Err(e)
                    }
                }
            }
            Backend::Postgres(pg) => {
                use sqlx::Executor;
                let mut conn = pg.block(pg.pool.acquire())?;
                pg.block(async {
                    conn.execute("BEGIN ISOLATION LEVEL READ COMMITTED")
                        .await
                        .map(|_| ())
                })?;
                let r = (|| {
                    let mut ex = PgExec {
                        pg,
                        conn: &mut conn,
                    };
                    if let Some(l) = lock {
                        ex.exec("SELECT pg_advisory_xact_lock(?1)", &[V::I(l.key())])?;
                    }
                    f(&mut ex)
                })();
                let out = match r {
                    Ok(v) => pg
                        .block(async { conn.execute("COMMIT").await.map(|_| ()) })
                        .map(|_| v)
                        .map_err(anyhow::Error::from),
                    Err(e) => {
                        let _ = pg.block(async { conn.execute("ROLLBACK").await.map(|_| ()) });
                        Err(e)
                    }
                };
                pg.release(conn);
                out
            }
        }
    }

    /// Raw multi-statement batch (migrations, tests that simulate an attacker).
    pub fn batch(&self, sql: &str) -> Result<()> {
        match self {
            Backend::Sqlite(c) => Ok(c
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .execute_batch(sql)?),
            Backend::Postgres(pg) => {
                let sql = sql.to_string();
                pg.block(async { sqlx::raw_sql(&sql).execute(&pg.pool).await.map(|_| ()) })?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholders_rewritten_outside_strings() {
        assert_eq!(
            super::pg_sql("SELECT '?1', x FROM t WHERE a=?1 AND b=?12"),
            "SELECT '?1', x FROM t WHERE a=$1 AND b=$12"
        );
    }
}
