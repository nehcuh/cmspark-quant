//! Persistent paper session: engine + append-only event log + snapshots in the
//! cmsq store (PostgreSQL in production), with audit entries per flush.
use crate::{Event, PaperEngine};
use anyhow::{ensure, Context, Result};
use cmsq_core::models::{Strategy, StrategyStatus};
use cmsq_core::Store;
use serde_json::json;

pub struct PaperSession<'a> {
    store: &'a Store,
    pub id: String,
    pub strategy_id: String,
    pub engine: PaperEngine,
}

fn kind(e: &Event) -> &'static str {
    match e {
        Event::Accepted { .. } => "accepted",
        Event::Rejected { .. } => "rejected",
        Event::Filled { .. } => "filled",
        Event::Cancelled { .. } => "cancelled",
        Event::Settled { .. } => "settled",
        Event::Marked { .. } => "marked",
    }
}

impl<'a> PaperSession<'a> {
    /// Start a session for a PROMOTED strategy.
    pub fn start(store: &'a Store, id: &str, s: &Strategy, engine: PaperEngine) -> Result<Self> {
        ensure!(
            s.status == StrategyStatus::Promoted,
            "strategy {} is {}, only Promoted strategies may paper-trade",
            s.id,
            s.status
        );
        store.create_paper_session(id, &s.id, &serde_json::to_value(&engine.cfg)?)?;
        store.audit(
            "paper",
            "paper_session_start",
            Some(&s.id),
            json!({"session": id, "config": engine.cfg, "simulated": true}),
            true,
        )?;
        let mut me = Self {
            store,
            id: id.into(),
            strategy_id: s.id.clone(),
            engine,
        };
        me.checkpoint()?;
        Ok(me)
    }

    /// Resume from the latest snapshot after verifying the stored event chain.
    pub fn resume(store: &'a Store, id: &str) -> Result<Self> {
        let (_, strategy_id, ..) = store
            .list_paper_sessions()?
            .into_iter()
            .find(|r| r.0 == id)
            .context("no such paper session")?;
        let (seq, state) = store
            .latest_paper_snapshot(id)?
            .context("session has no snapshot")?;
        verify_snapshot_binding(store, id, seq, &state)?;
        let engine: PaperEngine = serde_json::from_value(state)?;
        ensure!(engine.event_seq as i64 == seq, "snapshot seq mismatch");
        let head = verify_chain(store, id)?;
        // events after the snapshot are allowed (crash between flush and checkpoint) only if
        // the snapshot's chain is a prefix: re-verify at the snapshot's seq
        let at = chain_at(store, id, seq)?;
        ensure!(
            at == engine.event_chain,
            "snapshot chain does not match the stored event log"
        );
        ensure!(
            head.0 == seq,
            "events after the last snapshot ({} > {seq}): state would be stale; refusing to resume",
            head.0
        );
        Ok(Self {
            store,
            id: id.into(),
            strategy_id,
            engine,
        })
    }

    /// Persist pending events (append-only, seq-checked) and audit the batch.
    pub fn flush(&mut self) -> Result<usize> {
        let evs = self.engine.take_events();
        if evs.is_empty() {
            return Ok(0);
        }
        let mut prev = if evs[0].0 == 1 {
            String::new()
        } else {
            chain_at(self.store, &self.id, evs[0].0 as i64 - 1)?
        };
        let mut rows = vec![];
        let mut fills = 0;
        let mut settled = false;
        for (seq, e) in &evs {
            prev = PaperEngine::chain_step(&prev, e);
            fills += matches!(e, Event::Filled { .. }) as usize;
            settled |= matches!(e, Event::Settled { .. });
            rows.push((
                *seq as i64,
                kind(e).to_string(),
                serde_json::to_value(e)?,
                prev.clone(),
            ));
        }
        ensure!(
            prev == self.engine.event_chain,
            "event chain diverged from the engine"
        );
        self.store.append_paper_events(&self.id, &rows)?;
        self.store.audit("paper", "paper_events", Some(&self.strategy_id), json!({
            "session": self.id, "from": evs[0].0, "to": evs.last().unwrap().0, "fills": fills, "chain": prev, "simulated": true}), true)?;
        if settled {
            self.checkpoint()?;
        }
        Ok(rows.len())
    }

    /// Flush, then store a full state snapshot bound to the keyed audit chain.
    ///
    /// Snapshots are append-only and keyed by event seq. If only quotes moved
    /// since the last snapshot (marks / cumulative volume, issue #39), a `Marked`
    /// event is emitted first so that state gets its own seq instead of being lost.
    pub fn checkpoint(&mut self) -> Result<()> {
        if !self.engine.events().is_empty() {
            self.flush()?;
        }
        if let Some((seq, prev)) = self.store.latest_paper_snapshot(&self.id)? {
            if seq == self.engine.event_seq as i64 {
                if prev == serde_json::to_value(&self.engine)? {
                    return Ok(()); // identical state already stored
                }
                self.engine.record_marks();
                self.flush()?;
                if self
                    .store
                    .latest_paper_snapshot(&self.id)?
                    .is_some_and(|(s, _)| s == self.engine.event_seq as i64)
                {
                    return Ok(());
                }
            }
        }
        let e = &self.engine;
        let state = serde_json::to_value(e)?;
        let seq = e.event_seq as i64;
        self.store.insert_paper_snapshot(
            &self.id,
            seq,
            e.account.trading_day.as_deref(),
            e.equity(),
            &state,
        )?;
        // Bind (session, seq, full state digest) into the HMAC audit chain (issue #38):
        // a DB owner who edits state_json without the audit key cannot re-bind it.
        self.store.audit(
            "paper",
            "paper_snapshot",
            Some(&snapshot_target(&self.id)),
            json!({"session": self.id, "seq": seq, "state_sha256": state_digest(&self.id, seq, &state),
                   "chain": e.event_chain, "simulated": true}),
            true,
        )?;
        Ok(())
    }

    pub fn stop(mut self) -> Result<()> {
        self.checkpoint()?;
        self.store.set_paper_session_status(&self.id, "stopped")?;
        self.store.audit("paper", "paper_session_stop", Some(&self.strategy_id), json!({"session": self.id, "equity": self.engine.equity(), "chain": self.engine.event_chain}), true)?;
        Ok(())
    }
}

fn snapshot_target(id: &str) -> String {
    format!("paper-session:{id}")
}

/// sha256 over (session id, seq, canonical state JSON).
pub fn state_digest(id: &str, seq: i64, state: &serde_json::Value) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(id.as_bytes());
    h.update([0u8]);
    h.update(seq.to_string().as_bytes());
    h.update([0u8]);
    h.update(serde_json::to_vec(state).unwrap_or_default());
    hex::encode(h.finalize())
}

/// Refuse a snapshot unless the keyed audit chain verifies and its newest
/// `paper_snapshot` entry for this session binds exactly this (seq, state).
/// Legacy snapshots written before the binding existed are refused, never
/// silently re-authenticated: start a new session (the event log stays verifiable).
pub fn verify_snapshot_binding(
    store: &Store,
    id: &str,
    seq: i64,
    state: &serde_json::Value,
) -> Result<()> {
    let rep = store.verify_audit()?;
    ensure!(
        rep.ok,
        "audit chain does not verify; refusing to trust paper snapshots"
    );
    let entry = store
        .audit_entries(Some(&snapshot_target(id)), 1_000_000)?
        .into_iter()
        .find(|a| a.action == "paper_snapshot")
        .context(
            "snapshot is not bound to the audit chain (legacy or forged); refusing to resume",
        )?;
    ensure!(
        entry.details["session"] == id && entry.details["seq"] == seq,
        "latest snapshot (seq {seq}) is not the one bound in the audit chain"
    );
    ensure!(
        entry.details["state_sha256"] == state_digest(id, seq, state),
        "snapshot state does not match its audited digest (tampered); refusing to resume"
    );
    Ok(())
}

/// One equity-curve point. `authenticated` = this row's own state digest is bound
/// in the HMAC audit chain AND its derived equity/trading_day columns equal what the
/// authenticated state implies. `equity` is always recomputed from the state when
/// authenticated; for an unauthenticated point it is the raw (untrusted) column.
#[derive(Debug, Clone, PartialEq)]
pub struct CurvePoint {
    pub seq: i64,
    pub trading_day: Option<String>,
    pub equity: f64,
    pub authenticated: bool,
}

/// Rebuild the equity curve from per-snapshot authenticated state (issue #38 follow-up).
/// Legacy (unbound) or edited points come back with `authenticated = false`; callers
/// must not display them as account history.
pub fn verified_equity_curve(store: &Store, id: &str) -> Result<Vec<CurvePoint>> {
    let audit_ok = store.verify_audit()?.ok;
    let mut bound = std::collections::HashMap::new();
    if audit_ok {
        for a in store.audit_entries(Some(&snapshot_target(id)), 1_000_000)? {
            if a.action == "paper_snapshot" && a.details["session"] == id {
                if let (Some(seq), Some(d)) = (
                    a.details["seq"].as_i64(),
                    a.details["state_sha256"].as_str(),
                ) {
                    bound.entry(seq).or_insert_with(|| d.to_string());
                }
            }
        }
    }
    Ok(store
        .paper_snapshot_rows(id)?
        .into_iter()
        .map(|(seq, day, col_eq, state)| {
            let auth_state = bound
                .get(&seq)
                .is_some_and(|d| *d == state_digest(id, seq, &state))
                .then(|| serde_json::from_value::<PaperEngine>(state).ok())
                .flatten()
                .filter(|e| e.event_seq as i64 == seq);
            match auth_state {
                Some(e) => {
                    let eq = e.equity();
                    let ok = (eq - col_eq).abs() < 1e-6 && e.account.trading_day == day;
                    CurvePoint {
                        seq,
                        trading_day: if ok { e.account.trading_day } else { day },
                        equity: if ok { eq } else { col_eq },
                        authenticated: ok,
                    }
                }
                None => CurvePoint {
                    seq,
                    trading_day: day,
                    equity: col_eq,
                    authenticated: false,
                },
            }
        })
        .collect())
}

fn chain_at(store: &Store, id: &str, seq: i64) -> Result<String> {
    if seq == 0 {
        return Ok(String::new());
    }
    let r = store.list_paper_events(id, seq - 1, 1)?;
    r.first()
        .filter(|x| x.0 == seq)
        .map(|x| x.3.clone())
        .context("missing paper event")
}

/// Recompute the whole stored chain; returns (last seq, head hash).
pub fn verify_chain(store: &Store, id: &str) -> Result<(i64, String)> {
    let mut prev = String::new();
    let mut last = 0;
    loop {
        let page = store.list_paper_events(id, last, 1000)?;
        if page.is_empty() {
            break;
        }
        for (seq, _, ev, chain) in page {
            ensure!(seq == last + 1, "gap at {seq}");
            let e: Event = serde_json::from_value(ev).context("bad stored event")?;
            prev = PaperEngine::chain_step(&prev, &e);
            ensure!(
                prev == chain,
                "paper event {seq} was altered{}",
                if std::env::var("CMSQ_DEBUG_PAPER").is_ok() {
                    format!(": {}", serde_json::to_string(&e).unwrap())
                } else {
                    String::new()
                }
            );
            last = seq;
        }
    }
    Ok((last, prev))
}
