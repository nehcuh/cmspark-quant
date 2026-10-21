//! External, tamper-evident timestamping of the audit chain head through git.
//!
//! `publish` appends `<chain> <seq> <head mac> <utc ts>` to `anchors/<chain>.log` in
//! a git repository (ideally a dedicated repo with a remote on another host) and
//! commits it (GPG/SSH-signed with `sign=true`, using the operator's git signing
//! config), then optionally pushes. Git manages ANCHORS, never the key.
//! `verify` walks every anchor line ever committed and checks the database still has
//! exactly that MAC at that seq, so any later rewrite of history is detected, and
//! the git commit/push time bounds when each head existed.
use anyhow::{bail, ensure, Context, Result};
use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("git not found")?;
    if !o.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}

fn safe_chain(chain: &str) -> Result<&str> {
    ensure!(
        !chain.is_empty()
            && chain.len() <= 64
            && chain
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "bad chain id"
    );
    Ok(chain)
}

/// Append and commit one anchor. Returns the commit id. No-op (Ok(None)) if the head
/// is already the last anchor.
pub fn publish(
    repo: &Path,
    chain: &str,
    seq: i64,
    mac: &str,
    sign: bool,
    push: bool,
) -> Result<Option<String>> {
    let chain = safe_chain(chain)?;
    ensure!(
        mac.len() == 64 && mac.chars().all(|c| c.is_ascii_hexdigit()),
        "bad mac"
    );
    ensure!(
        repo.join(".git").exists(),
        "{} is not a git repository (git init it, add a remote on another host)",
        repo.display()
    );
    let rel = format!("anchors/{chain}.log");
    let f = repo.join(&rel);
    std::fs::create_dir_all(f.parent().unwrap())?;
    let existing = std::fs::read_to_string(&f).unwrap_or_default();
    if let Some(last) = existing.lines().last() {
        let p: Vec<&str> = last.split_whitespace().collect();
        if p.len() >= 3 && p[1] == seq.to_string() && p[2] == mac {
            return Ok(None);
        }
        if let Some(ls) = p.get(1).and_then(|x| x.parse::<i64>().ok()) {
            ensure!(
                seq >= ls,
                "audit head seq {seq} is behind the last published anchor {ls}: log truncated?"
            );
        }
    }
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    std::fs::write(&f, format!("{existing}{chain} {seq} {mac} {ts}\n"))?;
    git(repo, &["add", &rel])?;
    let msg = format!("audit anchor {chain} seq {seq}");
    let mut args = vec![
        "-c",
        "user.name=cmsq-anchor",
        "-c",
        "user.email=cmsq-anchor@localhost",
        "commit",
        "-q",
        "-m",
        &msg,
    ];
    if sign {
        args.push("-S");
    }
    // keep a configured identity if there is one
    if git(repo, &["config", "user.email"]).is_ok() {
        args.drain(0..4);
    }
    git(repo, &args)?;
    if push {
        git(repo, &["push", "-q"])?;
    }
    Ok(Some(git(repo, &["rev-parse", "HEAD"])?.trim().to_string()))
}

/// Every anchor of `chain` across the whole git history (all versions of the file),
/// as (seq, mac, ts, commit).
pub fn history(repo: &Path, chain: &str) -> Result<Vec<(i64, String, String, String)>> {
    let chain = safe_chain(chain)?;
    let rel = format!("anchors/{chain}.log");
    let commits = git(repo, &["log", "--format=%H", "--", &rel])?;
    let mut out = std::collections::BTreeMap::new();
    for c in commits.lines().rev() {
        let body = git(repo, &["show", &format!("{c}:{rel}")]).unwrap_or_default();
        for l in body.lines() {
            let p: Vec<&str> = l.split_whitespace().collect();
            if p.len() == 4 && p[0] == chain {
                if let Ok(seq) = p[1].parse::<i64>() {
                    out.entry((seq, p[2].to_string()))
                        .or_insert((p[3].to_string(), c.to_string()));
                }
            }
        }
    }
    Ok(out
        .into_iter()
        .map(|((s, m), (t, c))| (s, m, t, c))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publish_and_history_detect_rewrites_of_the_log_file() {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]).unwrap();
        let m1 = "a".repeat(64);
        let m2 = "b".repeat(64);
        assert!(publish(d.path(), "c1", 3, &m1, false, false)
            .unwrap()
            .is_some());
        assert!(
            publish(d.path(), "c1", 3, &m1, false, false)
                .unwrap()
                .is_none(),
            "idempotent"
        );
        publish(d.path(), "c1", 7, &m2, false, false).unwrap();
        assert!(
            publish(d.path(), "c1", 5, &m2, false, false).is_err(),
            "head went backwards"
        );
        // someone rewrites the file in a later commit: history still has the original lines
        std::fs::write(d.path().join("anchors/c1.log"), "c1 7 ffff 2026\n").unwrap();
        git(
            d.path(),
            &[
                "-c",
                "user.name=x",
                "-c",
                "user.email=x@y",
                "commit",
                "-qam",
                "rewrite",
            ],
        )
        .unwrap();
        let h = history(d.path(), "c1").unwrap();
        assert!(h.iter().any(|x| x.0 == 3 && x.1 == m1) && h.iter().any(|x| x.0 == 7 && x.1 == m2));
        assert!(safe_chain("../x").is_err());
    }
}
