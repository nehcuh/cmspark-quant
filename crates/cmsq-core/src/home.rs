//! On-disk layout of a cmsq workspace ($CMSQ_HOME, default ./var).
//!
//! ```text
//! cmsq.db                    SQLite: metadata, state, jobs, evaluations, signals, audit chain
//! drop/                      CMspark scrape-result drop folder (watched by `cmsq ingest`)
//! drop/processed/            drop files after successful hand-over to Core
//! raw/<strategy-id>/post.json  archived original scrape (immutable)
//! strategies/<id>/strategy.py, extracted.json
//! strategies/<id>/jobs/<job-id>/{job/,out/,stdout.log,stderr.log}
//! data/<SYMBOL>.csv + .meta.json  market data (mounted READ-ONLY into the sandbox)
//! vendor/huquant/            pinned huquant trade calendar (READ-ONLY mount)
//! runner/                    runner script for the process (dev) backend (READ-ONLY mount)
//! ```
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const RUNNER_PY: &str = include_str!("../../../runner/run_backtest.py");
pub const HUQUANT_TRADE_DATES: &str = include_str!("../../../vendor/huquant/trade_dates.py");
/// sha256 of vendored huquant/trade_dates.py @ 928aff42a932888025599dd9503ec9ad7c9458f3
pub const HUQUANT_SHA256: &str = "0b1d1bf53a0e5f9d277cd0d67b513415336263fd181d5bbce0e895c8eebbacf5";

#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn from_env() -> Self {
        Self::new(std::env::var("CMSQ_HOME").unwrap_or_else(|_| "var".into()))
    }
    pub fn abs(&self) -> PathBuf {
        std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone())
    }
    pub fn db(&self) -> PathBuf {
        self.root.join("cmsq.db")
    }
    pub fn drop_dir(&self) -> PathBuf {
        self.root.join("drop")
    }
    pub fn raw_dir(&self, id: &str) -> PathBuf {
        self.root.join("raw").join(id)
    }
    pub fn strategy_dir(&self, id: &str) -> PathBuf {
        self.root.join("strategies").join(id)
    }
    pub fn job_dir(&self, sid: &str, jid: &str) -> PathBuf {
        self.strategy_dir(sid).join("jobs").join(jid)
    }
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }
    pub fn vendor_dir(&self) -> PathBuf {
        self.root.join("vendor").join("huquant")
    }
    pub fn runner_dir(&self) -> PathBuf {
        self.root.join("runner")
    }

    /// Create layout and write pinned read-only assets. Idempotent.
    pub fn init(&self) -> Result<()> {
        for d in [
            self.drop_dir().join("processed"),
            self.drop_dir().join("rejected"),
            self.root.join("raw"),
            self.root.join("strategies"),
            self.data_dir(),
            self.vendor_dir(),
            self.runner_dir(),
        ] {
            std::fs::create_dir_all(&d).with_context(|| format!("mkdir {}", d.display()))?;
        }
        let got = hex::encode(Sha256::digest(HUQUANT_TRADE_DATES.as_bytes()));
        anyhow::ensure!(got == HUQUANT_SHA256, "vendored huquant checksum mismatch: {got}");
        write_readonly(&self.vendor_dir().join("trade_dates.py"), HUQUANT_TRADE_DATES)?;
        write_readonly(&self.runner_dir().join("run_backtest.py"), RUNNER_PY)?;
        Ok(())
    }
}

fn write_readonly(p: &Path, content: &str) -> Result<()> {
    if p.exists() {
        if std::fs::read_to_string(p).ok().as_deref() == Some(content) {
            return Ok(());
        }
        let mut perm = std::fs::metadata(p)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(p, perm)?;
    }
    std::fs::write(p, content)?;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(p, perm)?;
    Ok(())
}
