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

    /// Flush, then store a full state snapshot.
    pub fn checkpoint(&mut self) -> Result<()> {
        if !self.engine.events().is_empty() {
            self.flush()?;
        }
        let e = &self.engine;
        if self
            .store
            .latest_paper_snapshot(&self.id)?
            .is_some_and(|(seq, _)| seq == e.event_seq as i64)
        {
            return Ok(()); // nothing new since the last snapshot (quotes alone are not state worth keeping)
        }
        self.store.insert_paper_snapshot(
            &self.id,
            e.event_seq as i64,
            e.account.trading_day.as_deref(),
            e.equity(),
            &serde_json::to_value(e)?,
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
