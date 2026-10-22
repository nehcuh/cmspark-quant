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
  - It also allows a retry of the head seq (for when a DB commit failed after signing), but only with a byte-identical message, so the latest entry cannot be replaced.
  - `verify` answers only yes or no.
  - So even a fully compromised app cannot obtain MACs to rewrite past entries.
- **Independent head:** the signer remembers each chain's head, and `audit verify` compares the database against it, which catches truncation even if the anchor file is wiped. A chain the signer has never seen is refused until an operator explicitly enrolls it (see "显式登记" below); there is no trust-on-first-use.
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
Use `cmsq audit migrate-to-signer` as described in "显式登记（取代首次信任）与迁移" below. Do not delete `audit.key` by hand: the command imports it as key 0, enrolls the verified chain head, re-verifies the whole chain through the signer socket, and only then removes the app-side key.

## Tested
- `signer::tests`: append-only, rotation, permission refusal
- `tests/audit_signer.rs`: edit, truncation and git-rewrite detection through the store
- `scripts/signer_e2e.sh`: a real separate OS user; the app user gets "permission denied" on the key, and a forged edit is detected

## 显式登记（取代首次信任）与迁移

签名进程不再自动接受没见过的审计链。新工作区先由运维（能写签名目录的用户）登记：

```bash
sudo env CMSQ_AUDIT_SIGNER=/run/cmsq-signer/sock cmsq --home /srv/cmsq audit signer-enroll --signer-dir /var/lib/cmsq-signer
sudo chown -R cmsq-signer:cmsq-signer /var/lib/cmsq-signer/enroll
```

登记会钉住链的当前链头（新链为 seq 0）。签名进程只接受从这个链头往后接，同一序号的重试必须与原消息逐字节相同，因此也无法替换最新一条。

已有 `audit.key` 的工作区用迁移命令：

```bash
cmsq --home /srv/cmsq audit migrate-to-signer --signer-dir /var/lib/cmsq-signer --socket /run/cmsq-signer/sock
```

它导入密钥为 key 0、按已验证的链头登记，然后**通过签名进程的 socket** 重新验证整条链；只有验证通过才覆写并删除应用侧的 `audit.key`。签名进程需要在导入后重启一次以加载 key 0（未加载时命令会拒绝并保留密钥，重跑即可，导入和登记都是幂等的）。部署默认使用签名进程模式（`CMSQ_AUDIT_SIGNER`）。

## 锚点仓库

锚点推送到私有仓库 `github.com/nehcuh/cmspark-quant-anchors`。部署时把它克隆到 `/srv/cmsq-anchors`，配置 SSH 提交签名（`gpg.format ssh`，`user.signingkey`），由 `deploy/systemd/cmsq-anchor.timer` 每小时执行 `audit anchor-git --sign --push`；`audit verify-git` 校验历史上所有锚点。签名公钥若要在 GitHub 上显示 Verified，需要把它作为 signing key 添加到账号。
