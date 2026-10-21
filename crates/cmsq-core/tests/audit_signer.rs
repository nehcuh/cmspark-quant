//! Audit key held by a separate signer process (here: a thread on a private socket).
use cmsq_core::audit::AuditConfig;
use cmsq_core::signer::{serve, SocketSigner};
use cmsq_core::Store;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

fn signer() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let kd = d.path().join("signer");
    std::fs::create_dir(&kd).unwrap();
    std::fs::set_permissions(&kd, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sock = d.path().join("s.sock");
    let (k, s) = (kd.clone(), sock.clone());
    std::thread::spawn(move || serve(&k, &s, &[]).unwrap());
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    (d, sock)
}

fn cfg(sock: &std::path::Path, anchor: std::path::PathBuf) -> AuditConfig {
    AuditConfig {
        key: vec![],
        anchor: Some(anchor),
        signer: Some(Arc::new(SocketSigner::new(sock))),
        chain: "ws1".into(),
    }
}

#[test]
fn store_audits_through_signer_and_detects_rewrite_and_truncation() {
    let (d, sock) = signer();
    let st = Store::open(
        &d.path().join("db.sqlite"),
        cfg(&sock, d.path().join("a/ws1.anchor")),
    )
    .unwrap();
    for i in 0..5 {
        st.audit("t", "x", None, json!({"i": i}), true).unwrap();
    }
    assert!(st.verify_audit().unwrap().ok);
    assert!(st.audit_key_source().contains("signer socket"));
    // the app process has no key material at all
    let c = cfg(&sock, d.path().join("a/ws1.anchor"));
    assert!(c.key.is_empty());
    // an attacker with DB write access edits an entry: MAC check (yes/no from signer) fails
    st.raw_batch("DROP TRIGGER audit_no_update").unwrap();
    st.raw_batch("UPDATE audit SET details='{\"i\":99}' WHERE seq=3")
        .unwrap();
    let r = st.verify_audit().unwrap();
    assert!(!r.ok && r.first_bad_seq == Some(3), "{r:?}");
    // ... and cannot get a fresh MAC for the old seq from the signer
    let msg = cmsq_core::audit::entry_message("x", 3, "t", "a", "b", None, "{}", true);
    assert!(c.mac_next(3, "x", &msg).is_err());
}

#[test]
fn signer_head_catches_truncation_even_without_anchor_file() {
    let (d, sock) = signer();
    let mut c = cfg(&sock, d.path().join("a/ws2.anchor"));
    c.anchor = None;
    c.chain = "ws2".into();
    let st = Store::open(&d.path().join("db2.sqlite"), c).unwrap();
    for i in 0..4 {
        st.audit("t", "x", None, json!({"i": i}), true).unwrap();
    }
    st.raw_batch("DROP TRIGGER audit_no_delete").unwrap();
    st.raw_batch("DELETE FROM audit WHERE seq>=3").unwrap();
    let r = st.verify_audit().unwrap();
    assert!(!r.ok && r.problem.unwrap().contains("signer head"));
}

#[test]
fn git_anchor_detects_later_rewrite() {
    let (d, sock) = signer();
    let repo = d.path().join("anchors-repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q"])
        .status()
        .unwrap()
        .success());
    let mut c = cfg(&sock, d.path().join("a/ws3.anchor"));
    c.chain = "ws3".into();
    let st = Store::open(&d.path().join("db3.sqlite"), c).unwrap();
    for i in 0..3 {
        st.audit("t", "x", None, json!({"i": i}), true).unwrap();
    }
    let r = st.verify_audit().unwrap();
    cmsq_core::gitanchor::publish(&repo, "ws3", r.entries, &r.head_hash, false, false).unwrap();
    assert!(st.verify_git_anchors(&repo).unwrap().1.is_empty());
    st.raw_batch("DROP TRIGGER audit_no_update").unwrap();
    st.raw_batch("UPDATE audit SET hash='00' WHERE seq=3")
        .unwrap();
    let (n, bad) = st.verify_git_anchors(&repo).unwrap();
    assert_eq!(n, 1);
    assert!(bad[0].contains("rewritten"), "{bad:?}");
}
