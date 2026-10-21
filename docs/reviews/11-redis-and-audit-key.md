# Review 11: Redis hot layer and audit key custody (adversarial)

| Attack / failure | Guard / test |
|---|---|
| Redis down stalls trading or md loops | 300 ms connect timeout plus 10 s back-off (`unreachable_redis_degrades_fast`) |
| Evidence lost with Redis | nothing of record goes there; PG writes come first |
| Redis password in logs | redacted (`password_is_redacted`) |
| Rate-limit budget split per process | shared atomic Lua bucket (live test, two clients) |
| App reads the audit key | separate OS user; e2e gets "permission denied" |
| Compromised app asks the signer to re-MAC history | append-only signer; message must encode seq/prev (`append_only_no_rewrite…`) |
| Signer as a MAC oracle via verify | verify returns only a bool |
| Truncate the DB and wipe the anchor file | signer head check (`signer_head_catches_truncation…`) |
| Rewrite the anchors file in git | verify-git reads the whole history (`publish_and_history…`) |
| Path traversal via chain id | `safe_chain` |
| Rotation breaks old verification | epochs (`rotation_keeps_old_segment_verifiable`) |

Limitations:
- Signer state is a local JSON file. If an attacker becomes the signer user, they can reset it; the git anchors are the defence against that.
- The single-entry retry window lets someone who controls the app replace the most recent entry before the next one is signed.
