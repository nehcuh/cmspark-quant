//! On-disk layout of a cmsq workspace ($CMSQ_HOME, default ./var).
//!
//! ```text
//! cmsq.db                    SQLite: metadata, state, jobs, evaluations, signals, audit chain
//! drop/                      CMspark scrape-result drop folder (watched by `cmsq ingest`)
//! drop/processed/            drop files after successful hand-over to Core
//! raw/<strategy-id>/post.json  archived original scrape (immutable)
//! strategies/<id>/strategy.py, extracted.json
//! strategies/<id>/jobs/<job-id>/{job/,out/,stdout.log,stderr.log}
//! data/<SYMBOL>.csv + .meta.json  market data (HOST ONLY: never mounted; each container
//!                            gets one host-prepared variant in its job dir)
//!
//! Secrets (outside $CMSQ_HOME, default ~/.config/cmsq or $CMSQ_SECRETS_DIR):
//! audit.key                  HMAC key for the audit chain (0600)
//! anchors/<home-id>.anchor   append-only audit head anchors
//! vendor/huquant/            pinned huquant trade calendar (READ-ONLY mount)
//! runner/                    runner script for the process (dev) backend (READ-ONLY mount)
//! ```
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const RUNNER_PY: &str = include_str!("../../../runner/signal_runner.py");
pub const VNPY_SHIM_PY: &str = include_str!("../../../runner/vnpy_shim.py");
pub const HUQUANT_TRADE_DATES: &str = include_str!("../../../vendor/huquant/trade_dates.py");
/// sha256 of vendored huquant/trade_dates.py @ 928aff42a932888025599dd9503ec9ad7c9458f3
pub const HUQUANT_SHA256: &str = "0b1d1bf53a0e5f9d277cd0d67b513415336263fd181d5bbce0e895c8eebbacf5";

#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
    /// Directory holding the audit key and anchors. Must be outside `root`.
    pub secrets: PathBuf,
}

pub fn default_secrets_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CMSQ_SECRETS_DIR") {
        return d.into();
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".config")
        });
    base.join("cmsq")
}

impl Home {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            secrets: default_secrets_dir(),
        }
    }
    pub fn with_secrets(root: impl Into<PathBuf>, secrets: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            secrets: secrets.into(),
        }
    }
    pub fn key_file(&self) -> PathBuf {
        std::env::var("CMSQ_AUDIT_KEY_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| self.secrets.join("audit.key"))
    }
    pub fn anchor_file(&self) -> PathBuf {
        let id = hex::encode(&Sha256::digest(self.abs().display().to_string().as_bytes())[..8]);
        self.secrets.join("anchors").join(format!("{id}.anchor"))
    }
    /// Load (or create, 0600) the audit HMAC key; refuse a key inside $CMSQ_HOME.
    pub fn audit_config(&self) -> Result<crate::audit::AuditConfig> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let anchor0 = self.anchor_file();
        let chain = anchor0
            .file_stem()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        // 1) separate signer process (key held by another OS user): the app never sees the key
        if let Ok(sock) = std::env::var("CMSQ_AUDIT_SIGNER") {
            if let Some(p) = anchor0.parent() {
                std::fs::create_dir_all(p)?;
            }
            return Ok(crate::audit::AuditConfig {
                key: Vec::new(),
                anchor: Some(anchor0),
                signer: Some(std::sync::Arc::new(crate::signer::SocketSigner::new(sock))),
                chain,
            });
        }
        // 2) key from the environment (e.g. systemd-creds / container secret injection)
        if let Ok(k) = std::env::var("CMSQ_AUDIT_KEY") {
            let key = hex::decode(k.trim()).context("CMSQ_AUDIT_KEY must be hex")?;
            anyhow::ensure!(key.len() >= 16, "CMSQ_AUDIT_KEY too short");
            if let Some(p) = anchor0.parent() {
                std::fs::create_dir_all(p)?;
            }
            return Ok(crate::audit::AuditConfig {
                key,
                anchor: Some(anchor0),
                signer: None,
                chain,
            });
        }
        // 3) key file outside CMSQ_HOME (default)
        let kf = self.key_file();
        let root = self.abs();
        let kabs = kf
            .parent()
            .and_then(|p| {
                std::fs::create_dir_all(p)
                    .ok()
                    .and(std::fs::canonicalize(p).ok())
            })
            .unwrap_or_default();
        anyhow::ensure!(
            !kabs.starts_with(&root),
            "audit key {} must live outside CMSQ_HOME {}",
            kf.display(),
            root.display()
        );
        let anchor = self.anchor_file();
        if let Some(p) = anchor.parent() {
            std::fs::create_dir_all(p)?;
            anyhow::ensure!(
                !std::fs::canonicalize(p)?.starts_with(&root),
                "audit anchors must live outside CMSQ_HOME"
            );
        }
        if !kf.exists() {
            let mut key = Vec::with_capacity(32);
            key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
            key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&kf)
                .with_context(|| format!("create audit key {}", kf.display()))?;
            std::io::Write::write_all(&mut f, hex::encode(&key).as_bytes())?;
        }
        let md = std::fs::metadata(&kf)?;
        anyhow::ensure!(
            md.permissions().mode() & 0o077 == 0,
            "audit key {} must be mode 0600",
            kf.display()
        );
        let key =
            hex::decode(std::fs::read_to_string(&kf)?.trim()).context("audit key must be hex")?;
        anyhow::ensure!(key.len() >= 16, "audit key too short");
        Ok(crate::audit::AuditConfig {
            key,
            anchor: Some(anchor),
            signer: None,
            chain,
        })
    }
    /// A DEMO workspace (file `DEMO` in the root): synthetic-validated, not evidence.
    /// CLI and dashboard show a banner; created only by scripts/paper_demo.sh.
    pub fn is_demo(&self) -> bool {
        self.root.join("DEMO").exists()
    }
    pub fn from_env() -> Self {
        Self::new(std::env::var("CMSQ_HOME").unwrap_or_else(|_| "var".into()))
    }
    pub fn abs(&self) -> PathBuf {
        std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone())
    }
    /// Database settings; one PostgreSQL schema per workspace unless overridden.
    pub fn db_config(&self) -> crate::store::DbConfig {
        let mut c = crate::store::DbConfig::resolve(&self.secrets);
        if c.schema.is_none() {
            c.schema = Some(self.schema_name());
        }
        c
    }

    /// Random id written once when a workspace is first created. It is part of the
    /// PostgreSQL schema name, so deleting a workspace and re-creating it at the
    /// SAME path gets a fresh schema instead of inheriting the old rows (whose
    /// audit chain was signed with a key that no longer exists).
    pub fn workspace_id_file(&self) -> PathBuf {
        self.root.join(".cmsq_workspace_id")
    }

    /// `ws_<hash(path)>` for legacy workspaces (no id file: their data stays
    /// reachable); `ws_<hash(path + id)>` for every workspace created since.
    pub fn schema_name(&self) -> String {
        let mut key = self.abs().display().to_string();
        if let Ok(id) = std::fs::read_to_string(self.workspace_id_file()) {
            let id = id.trim();
            if !id.is_empty() {
                key = format!("{key}\n{id}");
            }
        }
        format!("ws_{}", hex::encode(&Sha256::digest(key.as_bytes())[..6]))
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
        // a brand-new workspace (nothing created yet) gets its own schema identity
        let fresh = !self.root.join("strategies").exists();
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
        anyhow::ensure!(
            got == HUQUANT_SHA256,
            "vendored huquant checksum mismatch: {got}"
        );
        write_readonly(
            &self.vendor_dir().join("trade_dates.py"),
            HUQUANT_TRADE_DATES,
        )?;
        write_readonly(&self.runner_dir().join("signal_runner.py"), RUNNER_PY)?;
        write_readonly(&self.runner_dir().join("vnpy_shim.py"), VNPY_SHIM_PY)?;
        let _ = std::fs::remove_file(self.runner_dir().join("run_backtest.py"));
        if fresh && !self.workspace_id_file().exists() {
            std::fs::write(self.workspace_id_file(), uuid::Uuid::new_v4().to_string())?;
        }
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

#[cfg(test)]
mod workspace_schema_tests {
    use super::*;

    #[test]
    fn recreated_workspace_at_same_path_gets_a_new_schema() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("ws");
        let h = Home::new(&root);
        h.init().unwrap();
        let first = h.schema_name();
        assert!(h.workspace_id_file().exists());
        h.init().unwrap(); // idempotent: same schema
        assert_eq!(h.schema_name(), first);
        std::fs::remove_dir_all(&root).unwrap();
        let h2 = Home::new(&root);
        h2.init().unwrap();
        assert_ne!(
            h2.schema_name(),
            first,
            "stale schema reused after re-create"
        );
    }

    #[test]
    fn legacy_workspace_without_id_keeps_its_schema() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("legacy");
        std::fs::create_dir_all(root.join("strategies")).unwrap();
        let h = Home::new(&root);
        let before = h.schema_name();
        h.init().unwrap();
        assert!(!h.workspace_id_file().exists());
        assert_eq!(h.schema_name(), before);
    }
}
