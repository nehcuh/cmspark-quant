//! Audit signer: holds the audit MAC key in a SEPARATE process (run it as its own OS
//! user; the key directory is readable only by that user). The app talks to it over a
//! unix socket and can ask for MACs of the NEXT chain entry, or a yes/no verification,
//! but can never read the key.
//!
//! Append-only by construction: per chain the signer remembers its own head and signs
//! only `seq == head+1 && prev == head_mac` (or a retry of the head itself, for a DB
//! commit that failed after signing). So even an attacker who controls the app cannot
//! get MACs to rewrite history; `verify` answers yes/no and never reveals a MAC.
//! A chain the signer has never seen is REFUSED: the operator must pin it first with
//! `cmsq audit signer-enroll` (writes `enroll/<chain>.json` into the signer directory,
//! which the app cannot write). The enrollment pins the chain's current head (seq, mac;
//! seq 0 = empty chain), so a compromised app can neither adopt a forged chain nor start
//! a second one. A head retry is only answered for the byte-identical message (hash
//! pinned), closing the "replace the latest entry" window.
//!
//! Key rotation: `cmsq signer rotate` adds key N+1 to the key directory. Each chain
//! records epochs (from_seq, key id); the next entry starts a new epoch under the newest
//! key, old entries keep verifying under their epoch's key. Retire an old key only
//! after its segment's head is anchored externally (git anchors).
//!
//! Protocol: one JSON object per line.
//!   {"op":"sign","chain":c,"seq":n,"prev":h,"msg":hex}       -> {"ok":true,"mac":hex}
//!   {"op":"verify","chain":c,"seq":n,"msg":hex,"mac":hex}    -> {"ok":true,"valid":bool}
//!   {"op":"head","chain":c}                                  -> {"ok":true,"head":[n,hex]|null}
//!   {"op":"info"}                                            -> {"ok":true,"keys":[ids],"chains":n}
use crate::audit::{ct_eq, hmac_hex, KeyProvider, GENESIS};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------- client ----------------

pub struct SocketSigner {
    path: PathBuf,
}

impl SocketSigner {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    fn call(&self, req: Value) -> Result<Value> {
        let mut s = UnixStream::connect(&self.path)
            .with_context(|| format!("audit signer {} unreachable", self.path.display()))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut line = req.to_string();
        line.push('\n');
        s.write_all(line.as_bytes())?;
        let mut out = String::new();
        BufReader::new(s).read_line(&mut out)?;
        let v: Value = serde_json::from_str(&out).context("bad signer reply")?;
        if v["ok"] != true {
            bail!(
                "audit signer refused: {}",
                v["error"].as_str().unwrap_or("?")
            );
        }
        Ok(v)
    }
}

impl KeyProvider for SocketSigner {
    fn describe(&self) -> String {
        format!("signer socket {}", self.path.display())
    }
    fn sign_next(&self, chain: &str, seq: i64, prev: &str, msg: &[u8]) -> Result<String> {
        let v = self.call(json!({"op": "sign", "chain": chain, "seq": seq, "prev": prev, "msg": hex::encode(msg)}))?;
        Ok(v["mac"].as_str().context("no mac")?.to_string())
    }
    fn verify(&self, chain: &str, seq: i64, msg: &[u8], mac: &str) -> Result<bool> {
        Ok(self.call(json!({"op": "verify", "chain": chain, "seq": seq, "msg": hex::encode(msg), "mac": mac}))?["valid"] == true)
    }
    fn head(&self, chain: &str) -> Result<Option<(i64, String)>> {
        let v = self.call(json!({"op": "head", "chain": chain}))?;
        Ok(v["head"].as_array().map(|a| {
            (
                a[0].as_i64().unwrap_or(0),
                a[1].as_str().unwrap_or("").to_string(),
            )
        }))
    }
}

// ---------------- server ----------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChainState {
    pub seq: i64,
    pub mac: String,
    /// prev of the head entry (to allow an idempotent retry of the head)
    pub prev: String,
    /// (from_seq, key id), ascending
    pub epochs: Vec<(i64, u32)>,
    /// sha256 of the head message: a retry must be byte-identical
    #[serde(default)]
    pub msg_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enrollment {
    pub chain: String,
    /// current head seq (0 = empty chain, next entry is seq 1 from GENESIS)
    pub seq: i64,
    /// current head mac (GENESIS when seq == 0)
    pub mac: String,
}

fn valid_chain(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 128
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub struct SignerCore {
    dir: PathBuf,
    keys: BTreeMap<u32, Vec<u8>>,
    state: BTreeMap<String, ChainState>,
}

fn check_private(p: &Path, want_dir: bool) -> Result<()> {
    let md = std::fs::metadata(p).with_context(|| format!("{}", p.display()))?;
    ensure!(
        md.is_dir() == want_dir,
        "{} has the wrong type",
        p.display()
    );
    ensure!(
        md.permissions().mode() & 0o077 == 0,
        "{} must not be accessible to group/others (chmod {})",
        p.display(),
        if want_dir { "700" } else { "600" }
    );
    Ok(())
}

impl SignerCore {
    /// `dir` holds `keys/<id>.key` (hex) and `state.json`; must be mode 0700.
    pub fn open(dir: &Path) -> Result<Self> {
        check_private(dir, true)?;
        let kd = dir.join("keys");
        if !kd.exists() {
            std::fs::create_dir(&kd)?;
            std::fs::set_permissions(&kd, std::fs::Permissions::from_mode(0o700))?;
        }
        check_private(&kd, true)?;
        let mut keys = BTreeMap::new();
        for e in std::fs::read_dir(&kd)? {
            let p = e?.path();
            let Some(id) = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".key"))
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            check_private(&p, false)?;
            let k = hex::decode(std::fs::read_to_string(&p)?.trim()).context("key must be hex")?;
            ensure!(k.len() >= 32, "key {id} too short");
            keys.insert(id, k);
        }
        let sf = dir.join("state.json");
        let state = if sf.exists() {
            serde_json::from_str(&std::fs::read_to_string(&sf)?)?
        } else {
            BTreeMap::new()
        };
        let mut me = Self {
            dir: dir.into(),
            keys,
            state,
        };
        if me.keys.is_empty() {
            me.rotate()?;
        }
        Ok(me)
    }
    /// Import an existing in-process key as key 0 (migration from audit.key).
    pub fn import(dir: &Path, key_hex: &str) -> Result<()> {
        check_private(dir, true)?;
        let kd = dir.join("keys");
        std::fs::create_dir_all(&kd)?;
        std::fs::set_permissions(&kd, std::fs::Permissions::from_mode(0o700))?;
        let k = hex::decode(key_hex.trim()).context("hex")?;
        ensure!(k.len() >= 16, "key too short");
        let p = kd.join("0.key");
        if p.exists() {
            // idempotent re-run of a migration; a DIFFERENT key 0 is refused
            ensure!(
                std::fs::read_to_string(&p)?.trim() == hex::encode(&k),
                "a different key 0 already exists"
            );
            return Ok(());
        }
        write_private(&p, hex::encode(&k).as_bytes())
    }
    /// Operator-side: pin a chain at its current head. Needs write access to the signer
    /// directory (the app has none). Works while the signer is running.
    pub fn enroll(dir: &Path, e: &Enrollment) -> Result<()> {
        check_private(dir, true)?;
        ensure!(valid_chain(&e.chain), "bad chain id");
        ensure!(e.seq >= 0, "bad seq");
        ensure!(
            (e.seq == 0) == (e.mac == GENESIS),
            "seq 0 iff mac == GENESIS"
        );
        let ed = dir.join("enroll");
        if !ed.exists() {
            std::fs::create_dir(&ed)?;
            std::fs::set_permissions(&ed, std::fs::Permissions::from_mode(0o700))?;
        }
        check_private(&ed, true)?;
        let p = ed.join(format!("{}.json", e.chain));
        if p.exists() {
            let old: Enrollment = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
            ensure!(
                old.seq == e.seq && old.mac == e.mac,
                "chain {} already enrolled at a different head",
                e.chain
            );
            return Ok(());
        }
        write_private(&p, serde_json::to_string(e)?.as_bytes())
    }
    fn enrollment(&self, chain: &str) -> Result<Option<Enrollment>> {
        if !valid_chain(chain) {
            return Ok(None);
        }
        let p = self.dir.join("enroll").join(format!("{chain}.json"));
        if !p.exists() {
            return Ok(None);
        }
        check_private(&self.dir.join("enroll"), true)?;
        check_private(&p, false)?;
        let e: Enrollment = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
        ensure!(e.chain == chain, "enrollment file mismatch");
        Ok(Some(e))
    }
    pub fn rotate(&mut self) -> Result<u32> {
        let id = self.keys.keys().next_back().map(|x| x + 1).unwrap_or(1);
        let mut k = vec![0u8; 32];
        getrandom(&mut k)?;
        write_private(
            &self.dir.join("keys").join(format!("{id}.key")),
            hex::encode(&k).as_bytes(),
        )?;
        self.keys.insert(id, k);
        Ok(id)
    }
    fn current(&self) -> u32 {
        *self.keys.keys().next_back().unwrap()
    }
    fn persist(&self) -> Result<()> {
        let tmp = self.dir.join("state.json.tmp");
        write_private(&tmp, serde_json::to_string_pretty(&self.state)?.as_bytes())?;
        std::fs::rename(tmp, self.dir.join("state.json"))?;
        Ok(())
    }
    fn key_for(&self, st: Option<&ChainState>, seq: i64) -> Option<&Vec<u8>> {
        let id = st
            .and_then(|s| s.epochs.iter().rev().find(|e| e.0 <= seq).map(|e| e.1))
            .unwrap_or_else(|| *self.keys.keys().next().unwrap());
        self.keys.get(&id)
    }

    pub fn handle(&mut self, req: &Value) -> Value {
        match self.handle_inner(req) {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
        }
    }
    fn handle_inner(&mut self, req: &Value) -> Result<Value> {
        let chain = req["chain"].as_str().unwrap_or("").to_string();
        ensure!(chain.len() <= 128, "chain id too long");
        match req["op"].as_str().unwrap_or("") {
            "sign" => {
                let seq = req["seq"].as_i64().context("seq")?;
                let prev = req["prev"].as_str().context("prev")?.to_string();
                let msg = hex::decode(req["msg"].as_str().context("msg")?)?;
                ensure!(msg.len() <= 1 << 20, "message too large");
                // the message must really be for (seq, prev): its first two fields
                ensure!(
                    msg_fields_match(&msg, &prev, seq),
                    "message does not encode the claimed seq/prev"
                );
                let cur = self.current();
                if !self.state.contains_key(&chain) {
                    let Some(e) = self.enrollment(&chain)? else {
                        bail!("chain {chain:?} is not enrolled with this signer (run `cmsq audit signer-enroll`)");
                    };
                    ensure!(
                        seq == e.seq + 1 && prev == e.mac,
                        "refusing to sign seq {seq}: enrolled head is seq {}",
                        e.seq
                    );
                    self.state.insert(
                        chain.clone(),
                        ChainState {
                            seq: e.seq,
                            mac: e.mac.clone(),
                            prev: String::new(),
                            epochs: vec![],
                            msg_sha: String::new(),
                        },
                    );
                }
                let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&msg));
                let st = self.state.get_mut(&chain).unwrap();
                let next = st.seq + 1 == seq && st.mac == prev;
                let retry =
                    seq == st.seq && prev == st.prev && !st.msg_sha.is_empty() && st.msg_sha == sha;
                ensure!(
                    next || retry,
                    "refusing to sign seq {seq}: chain head is seq {} (signer is append-only; a head retry must be byte-identical)",
                    st.seq
                );
                if st.epochs.last().map(|e| e.1) != Some(cur) {
                    st.epochs.retain(|e| e.0 < seq);
                    st.epochs.push((seq, cur));
                }
                let mac = hmac_hex(&self.keys[&cur], &msg);
                *st = ChainState {
                    seq,
                    mac: mac.clone(),
                    prev,
                    epochs: st.epochs.clone(),
                    msg_sha: sha,
                };
                self.persist()?;
                Ok(json!({"ok": true, "mac": mac}))
            }
            "verify" => {
                let seq = req["seq"].as_i64().context("seq")?;
                let msg = hex::decode(req["msg"].as_str().context("msg")?)?;
                let mac = req["mac"].as_str().context("mac")?;
                let st = self.state.get(&chain);
                let valid = match self.key_for(st, seq) {
                    Some(k) => ct_eq(hmac_hex(k, &msg).as_bytes(), mac.as_bytes()),
                    None => false,
                };
                Ok(json!({"ok": true, "valid": valid}))
            }
            "head" => Ok(
                json!({"ok": true, "head": self.state.get(&chain).filter(|s| s.seq > 0).map(|s| json!([s.seq, s.mac]))}),
            ),
            "info" => Ok(
                json!({"ok": true, "keys": self.keys.keys().collect::<Vec<_>>(), "chains": self.state.len()}),
            ),
            op => bail!("unknown op {op:?}"),
        }
    }
}

/// The canonical message starts with len-prefixed prev and seq (audit::entry_message).
fn msg_fields_match(msg: &[u8], prev: &str, seq: i64) -> bool {
    let mut i = 0;
    let mut field = || -> Option<&[u8]> {
        let n = u64::from_le_bytes(msg.get(i..i + 8)?.try_into().ok()?) as usize;
        let f = msg.get(i + 8..i + 8 + n)?;
        i += 8 + n;
        Some(f)
    };
    field() == Some(prev.as_bytes()) && field() == Some(seq.to_string().as_bytes())
}

fn write_private(p: &Path, b: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(p)?;
    f.write_all(b)?;
    f.sync_all()?;
    Ok(())
}

fn getrandom(buf: &mut [u8]) -> Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)?;
    Ok(())
}

/// Peer uid of a unix socket connection.
fn peer_uid(s: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

/// Serve forever. `allow_uids` empty = anyone who can open the socket (use file
/// permissions: socket mode 0660, group = the app's group).
pub fn serve(dir: &Path, socket: &Path, allow_uids: &[u32]) -> Result<()> {
    let core = Arc::new(Mutex::new(SignerCore::open(dir)?));
    if socket.exists() {
        std::fs::remove_file(socket)?;
    }
    let l = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))?;
    eprintln!(
        "cmsq signer: {} keys, listening on {}",
        core.lock().unwrap().keys.len(),
        socket.display()
    );
    for c in l.incoming() {
        let Ok(c) = c else { continue };
        if !allow_uids.is_empty() && !peer_uid(&c).is_some_and(|u| allow_uids.contains(&u)) {
            continue;
        }
        let core = core.clone();
        std::thread::spawn(move || {
            let _ = c.set_read_timeout(Some(Duration::from_secs(30)));
            let mut w = match c.try_clone() {
                Ok(w) => w,
                Err(_) => return,
            };
            let r = BufReader::new(c);
            for line in r.lines().take(10_000) {
                let Ok(line) = line else { break };
                if line.len() > 3 << 20 {
                    break;
                }
                let resp = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => core.lock().unwrap_or_else(|e| e.into_inner()).handle(&v),
                    Err(_) => json!({"ok": false, "error": "bad json"}),
                };
                if w.write_all(format!("{resp}\n").as_bytes()).is_err() {
                    break;
                }
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::entry_message;

    fn dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }
    fn sign(c: &mut SignerCore, seq: i64, prev: &str) -> Value {
        let m = entry_message(prev, seq, "t", "a", "x", None, "{}", true);
        c.handle(
            &json!({"op": "sign", "chain": "c", "seq": seq, "prev": prev, "msg": hex::encode(m)}),
        )
    }

    #[test]
    fn append_only_no_rewrite_no_mac_oracle_for_old_entries() {
        let d = dir();
        let mut c = SignerCore::open(d.path()).unwrap();
        enroll0(d.path());
        let m1 = sign(&mut c, 1, GENESIS)["mac"]
            .as_str()
            .unwrap()
            .to_string();
        let m2 = sign(&mut c, 2, &m1)["mac"].as_str().unwrap().to_string();
        // rewrite of entry 1 refused
        assert_eq!(sign(&mut c, 1, GENESIS)["ok"], false);
        // skipping ahead refused
        assert_eq!(sign(&mut c, 4, &m2)["ok"], false);
        // message that lies about seq/prev refused
        let m = entry_message(&m2, 9, "t", "a", "x", None, "{}", true);
        assert_eq!(
            c.handle(&json!({"op":"sign","chain":"c","seq":3,"prev":m2,"msg":hex::encode(m)}))
                ["ok"],
            false
        );
        // retry of the head (failed DB commit) allowed
        assert_eq!(sign(&mut c, 2, &m1)["ok"], true);
        // verify is yes/no
        let msg1 = entry_message(GENESIS, 1, "t", "a", "x", None, "{}", true);
        assert_eq!(
            c.handle(&json!({"op":"verify","chain":"c","seq":1,"msg":hex::encode(&msg1),"mac":m1}))
                ["valid"],
            true
        );
        assert_eq!(
            c.handle(
                &json!({"op":"verify","chain":"c","seq":1,"msg":hex::encode(&msg1),"mac":"00"})
            )["valid"],
            false
        );
        // state survives restart
        let c2 = SignerCore::open(d.path()).unwrap();
        assert_eq!(c2.state["c"].seq, 2);
    }

    #[test]
    fn rotation_keeps_old_segment_verifiable() {
        let d = dir();
        let mut c = SignerCore::open(d.path()).unwrap();
        enroll0(d.path());
        let m1 = sign(&mut c, 1, GENESIS)["mac"]
            .as_str()
            .unwrap()
            .to_string();
        let k2 = c.rotate().unwrap();
        let m2 = sign(&mut c, 2, &m1)["mac"].as_str().unwrap().to_string();
        assert_eq!(c.state["c"].epochs, vec![(1, k2 - 1), (2, k2)]);
        for (seq, prev, mac) in [(1, GENESIS.to_string(), m1.clone()), (2, m1.clone(), m2)] {
            let m = entry_message(&prev, seq, "t", "a", "x", None, "{}", true);
            assert_eq!(
                c.handle(
                    &json!({"op":"verify","chain":"c","seq":seq,"msg":hex::encode(m),"mac":mac})
                )["valid"],
                true,
                "seq {seq}"
            );
        }
    }

    fn enroll0(p: &Path) {
        SignerCore::enroll(
            p,
            &Enrollment {
                chain: "c".into(),
                seq: 0,
                mac: GENESIS.into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn unenrolled_chain_refused_and_enrollment_pins_head() {
        let d = dir();
        let mut c = SignerCore::open(d.path()).unwrap();
        // no TOFU: neither genesis nor mid-chain adoption without enrollment
        assert_eq!(sign(&mut c, 1, GENESIS)["ok"], false);
        assert_eq!(sign(&mut c, 7, "ab")["ok"], false);
        SignerCore::enroll(
            d.path(),
            &Enrollment {
                chain: "c".into(),
                seq: 6,
                mac: "ab".into(),
            },
        )
        .unwrap();
        // must continue exactly from the pinned head
        assert_eq!(sign(&mut c, 7, "cd")["ok"], false);
        assert_eq!(sign(&mut c, 1, GENESIS)["ok"], false);
        assert_eq!(sign(&mut c, 7, "ab")["ok"], true);
        // re-enrolling at a different head is refused (identical re-run is a no-op)
        assert!(SignerCore::enroll(
            d.path(),
            &Enrollment {
                chain: "c".into(),
                seq: 0,
                mac: GENESIS.into()
            }
        )
        .is_err());
    }

    #[test]
    fn head_retry_must_be_byte_identical() {
        let d = dir();
        let mut c = SignerCore::open(d.path()).unwrap();
        enroll0(d.path());
        let m1 = sign(&mut c, 1, GENESIS)["mac"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(sign(&mut c, 1, GENESIS)["mac"], m1.as_str());
        // a different message for the same (seq, prev) = replacing the head: refused
        let other = entry_message(GENESIS, 1, "t", "a", "FORGED", None, "{}", true);
        assert_eq!(
            c.handle(
                &json!({"op":"sign","chain":"c","seq":1,"prev":GENESIS,"msg":hex::encode(other)})
            )["ok"],
            false
        );
    }

    #[test]
    fn refuses_world_readable_key_dir() {
        let d = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(SignerCore::open(d.path()).is_err());
    }
}
