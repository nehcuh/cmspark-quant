# Audit key custody and external anchors

The key is **never in git**, and is never in the app's memory once a signer is used. Git stores only anchors, which are published chain heads.

## Key providers (`audit::KeyProvider`)
| Provider | How to select | Where the key is |
|---|---|---|
| signer socket (recommended) | `CMSQ_AUDIT_SIGNER=/run/cmsq-signer/sock` | `/var/lib/cmsq-signer/keys/<id>.key`, owned by user `cmsq-signer`, mode 0700/0600 |
| env | `CMSQ_AUDIT_KEY=<hex>` | injected by systemd-creds or a container secret |
| file (default, dev) | `<secrets>/audit.key` (0600, outside CMSQ_HOME) | the app user's config dir |

HSM and KMS fit the same trait: `sign_next`, `verify` (yes/no) and `head`.

## Signer (`cmsq signer serve`, `deploy/systemd/cmsq-signer.service`)
- Runs as its own OS user. The socket is mode 0660 (group `cmsq-sign`), with an optional `--allow-uid` check via SO_PEERCRED. The unit has no network.
- **Append-only:**
  - It signs only the next entry of a chain (`seq == head+1`, `prev == head mac`), and checks that the message really encodes that seq and prev.
  - It also allows one retry of the head, for the case where a DB commit failed after signing.
  - `verify` answers only yes or no.
  - So even a fully compromised app cannot obtain MACs to rewrite past entries.
- **Independent head:** the signer remembers each chain's head, and `audit verify` compares the database against it, which catches truncation even if the anchor file is wiped. A chain the signer has never seen is adopted on first use, so existing workspaces can migrate.
- **Residual risk:** someone controlling the app *now* can append bogus new entries. They cannot alter what has already been anchored.

## Git anchors (`cmsq audit anchor-git --repo R [--sign] [--push] [--every 1h]`)
Each run appends `chain seq head-mac utc-ts` to `anchors/<chain>.log` and commits it, as a signed commit when `--sign` is given. With `--push` it also pushes, so the remote, ideally on another host or a WORM-capable forge, timestamps each head.

`cmsq audit verify-git --repo R` checks every anchor line in the whole git history, so a later rewrite of the log file does not hide an old anchor. It confirms that each anchored seq still has the same MAC in the database. Run it hourly with `deploy/systemd/cmsq-anchor.{service,timer}`.

## Key rotation
1. `cmsq audit anchor-git --repo R --sign --push`: anchor the current head.
2. `cmsq signer rotate --dir /var/lib/cmsq-signer`: creates key N+1. Restart the signer.
3. The next entry of each chain starts a new epoch `(from_seq, N+1)`. Older entries keep verifying under their epoch's key.
4. You may move an old key offline only after its segment's head is anchored and pushed. From then on, that segment's integrity rests on the git anchors.

## Migrating an existing workspace
```
cmsq signer import --dir /var/lib/cmsq-signer --key-file ~/.config/cmsq/audit.key
```
This imports the old key as key 0. Start the signer, set `CMSQ_AUDIT_SIGNER`, then delete the old key file. The signer adopts the chain at its next entry, and new entries use the newest key.

## Tested
- `signer::tests`: append-only, rotation, permission refusal
- `tests/audit_signer.rs`: edit, truncation and git-rewrite detection through the store
- `scripts/signer_e2e.sh`: a real separate OS user; the app user gets "permission denied" on the key, and a forged edit is detected
