#!/usr/bin/env bash
# End-to-end: audit key held by a separate OS user (needs sudo; creates user cmsq-signer).
# The app user can request signatures over the socket but cannot read the key.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-$PWD/target/debug/cmsq}
id cmsq-signer >/dev/null 2>&1 || sudo useradd --system --no-create-home --shell /usr/sbin/nologin cmsq-signer
getent group cmsq-sign >/dev/null || sudo groupadd --system cmsq-sign
sudo usermod -aG cmsq-sign cmsq-signer
SD=/var/lib/cmsq-signer; RD=/run/cmsq-signer
sudo install -d -o cmsq-signer -g cmsq-signer -m 0700 $SD
sudo install -d -o cmsq-signer -g cmsq-sign -m 0750 $RD
sudo install -m 0755 "$BIN" /usr/local/bin/cmsq-signer-bin
sudo pkill -u cmsq-signer -f cmsq-signer-bin || true
sudo -u cmsq-signer -g cmsq-sign nohup /usr/local/bin/cmsq-signer-bin signer serve --dir $SD --socket $RD/sock --allow-uid "$(id -u)" >/tmp/cmsq-signer.log 2>&1 &
for _ in $(seq 50); do sudo test -S $RD/sock && break; sleep 0.1; done
# app user joins the socket group for this test (setfacl alternative)
sudo setfacl -m u:"$(id -u)":x $RD 2>/dev/null || sudo chmod 0751 $RD
sudo setfacl -m u:"$(id -u)":rw $RD/sock 2>/dev/null || sudo chmod 0666 $RD/sock
echo "--- app user cannot read the key:"
if cat $SD/keys/1.key 2>/dev/null || ls $SD 2>/dev/null; then echo "FAIL: key readable"; exit 1; else echo "ok: permission denied"; fi
H=$(mktemp -d /tmp/cmsq-signer-e2e.XXXX)
export CMSQ_DB=sqlite CMSQ_AUDIT_SIGNER=$RD/sock CMSQ_SECRETS_DIR=$H/secrets
# explicit enrollment (operator/root writes the signer dir; replaces trust-on-first-use)
read -r CH SEQ MAC < <("$BIN" --home $H/ws audit signer-enroll --print)
sudo -u cmsq-signer -g cmsq-sign /usr/local/bin/cmsq-signer-bin signer enroll --dir $SD --chain "$CH" --seq "$SEQ" --mac "$MAC"
"$BIN" --home $H/ws init >/dev/null
"$BIN" --home $H/ws audit verify
REPO=$H/anchors; git init -q $REPO
"$BIN" --home $H/ws audit anchor-git --repo $REPO
"$BIN" --home $H/ws audit verify-git --repo $REPO
[ ! -e $H/secrets/audit.key ] && echo "ok: no audit.key on the app side"
echo "--- forged rewrite via sqlite (app user) is detected:"
sqlite3 $H/ws/cmsq.db "DROP TRIGGER audit_no_update; UPDATE audit SET details='{}' WHERE seq=1;" 2>/dev/null || python3 - <<PY
import sqlite3; c=sqlite3.connect("$H/ws/cmsq.db"); c.execute("DROP TRIGGER audit_no_update"); c.execute("UPDATE audit SET details='{}' WHERE seq=1"); c.commit()
PY
if "$BIN" --home $H/ws audit verify; then echo "FAIL: tamper not detected"; exit 1; else echo "ok: tamper detected"; fi
if "$BIN" --home $H/ws audit verify-git --repo $REPO; then echo "(git anchor of head unaffected: edit was before head)"; fi
echo "SIGNER E2E PASSED"
