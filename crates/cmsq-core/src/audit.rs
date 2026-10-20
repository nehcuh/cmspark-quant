//! Tamper-evident audit chain: hash_n = sha256(prev_hash || canonical(entry_n)).
use sha2::{Digest, Sha256};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[allow(clippy::too_many_arguments)]
pub fn entry_hash(
    prev_hash: &str,
    seq: i64,
    ts: &str,
    actor: &str,
    action: &str,
    target: Option<&str>,
    details: &str,
    success: bool,
) -> String {
    let mut h = Sha256::new();
    for part in [
        prev_hash,
        &seq.to_string(),
        ts,
        actor,
        action,
        target.unwrap_or(""),
        details,
        if success { "1" } else { "0" },
    ] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyReport {
    pub entries: i64,
    pub ok: bool,
    pub first_bad_seq: Option<i64>,
    pub problem: Option<String>,
    pub head_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hash_changes_with_any_field() {
        let a = entry_hash(GENESIS, 1, "t", "u", "x", Some("id"), "{}", true);
        assert_ne!(a, entry_hash(GENESIS, 1, "t", "u", "x", Some("id"), "{}", false));
        assert_ne!(a, entry_hash(GENESIS, 2, "t", "u", "x", Some("id"), "{}", true));
        assert_ne!(a, entry_hash(GENESIS, 1, "t", "u", "x", Some("id2"), "{}", true));
        // length-prefixing prevents field-boundary ambiguity
        assert_ne!(
            entry_hash(GENESIS, 1, "t", "ab", "c", None, "{}", true),
            entry_hash(GENESIS, 1, "t", "a", "bc", None, "{}", true)
        );
    }
}
