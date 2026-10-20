//! Tamper-evident audit chain.
//!
//! mac_n = HMAC-SHA256(key, len-prefixed(prev_mac, seq, ts, actor, action, target, details, success))
//!
//! * The key lives OUTSIDE the database and outside $CMSQ_HOME
//!   (default `~/.config/cmsq/audit.key`, mode 0600), so someone who can write the
//!   DB cannot recompute a valid chain after editing it.
//! * After every append the head `(seq, mac)` is appended to an anchor file that
//!   also lives outside $CMSQ_HOME. Verification requires the DB to contain the
//!   anchored head, so deleting the tail (truncation) or replacing the DB with an
//!   older/forked copy is detected even though every remaining link is valid.
//!
//! Residual risk: an attacker who can read the key AND rewrite the anchor file can
//! still forge history. Ship anchors to a separate host/WORM storage to close it.
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone)]
pub struct AuditConfig {
    pub key: Vec<u8>,
    pub anchor: Option<PathBuf>,
}

impl std::fmt::Debug for AuditConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditConfig").field("key", &"<redacted>").field("anchor", &self.anchor).finish()
    }
}

impl AuditConfig {
    /// Fixed key, no anchor — unit tests only.
    pub fn insecure_test() -> Self {
        Self { key: b"cmsq-test-key-not-secret".to_vec(), anchor: None }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn entry_hash(key: &[u8], prev_hash: &str, seq: i64, ts: &str, actor: &str, action: &str, target: Option<&str>, details: &str, success: bool) -> String {
    let mut h = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    for part in [prev_hash, &seq.to_string(), ts, actor, action, target.unwrap_or(""), details, if success { "1" } else { "0" }] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize().into_bytes())
}

pub fn append_anchor(path: &Path, seq: i64, hash: &str, ts: &str) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{seq} {hash} {ts}")?;
    f.sync_data()
}

/// Last `(seq, hash)` in the anchor file.
pub fn read_anchor(path: &Path) -> std::io::Result<Option<(i64, String)>> {
    let s = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(s.lines().rev().find_map(|l| {
        let mut it = l.split_whitespace();
        Some((it.next()?.parse().ok()?, it.next()?.to_string()))
    }))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyReport {
    pub entries: i64,
    pub ok: bool,
    pub first_bad_seq: Option<i64>,
    pub problem: Option<String>,
    pub head_hash: String,
    pub keyed: bool,
    pub anchor: Option<String>,
    pub anchored_seq: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mac_changes_with_any_field_and_key() {
        let k = b"k1";
        let a = entry_hash(k, GENESIS, 1, "t", "u", "x", Some("id"), "{}", true);
        assert_ne!(a, entry_hash(k, GENESIS, 1, "t", "u", "x", Some("id"), "{}", false));
        assert_ne!(a, entry_hash(k, GENESIS, 2, "t", "u", "x", Some("id"), "{}", true));
        assert_ne!(a, entry_hash(k, GENESIS, 1, "t", "u", "x", Some("id2"), "{}", true));
        assert_ne!(a, entry_hash(b"k2", GENESIS, 1, "t", "u", "x", Some("id"), "{}", true), "keyed");
        assert_ne!(entry_hash(k, GENESIS, 1, "t", "ab", "c", None, "{}", true), entry_hash(k, GENESIS, 1, "t", "a", "bc", None, "{}", true));
    }
    #[test]
    fn anchor_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/x.anchor");
        assert_eq!(read_anchor(&p).unwrap(), None);
        append_anchor(&p, 1, "aa", "t").unwrap();
        append_anchor(&p, 2, "bb", "t").unwrap();
        assert_eq!(read_anchor(&p).unwrap(), Some((2, "bb".into())));
    }
}
