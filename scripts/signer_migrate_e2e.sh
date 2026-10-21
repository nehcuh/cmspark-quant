#!/usr/bin/env bash
# Same-user smoke test of: app-side audit.key workspace -> `audit migrate-to-signer`
# (import + enroll + verify through socket + shred), then normal operation via the signer,
# and refusal of an un-enrolled workspace. Production runs the signer as its own user.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-$PWD/target/debug/cmsq}
H=$(mktemp -d /tmp/cmsq-mig.XXXX); trap 'kill $SP 2>/dev/null; rm -rf $H' EXIT
export CMSQ_DB=sqlite CMSQ_SECRETS_DIR=$H/secrets
unset CMSQ_AUDIT_SIGNER CMSQ_AUDIT_KEY
"$BIN" --home $H/ws init >/dev/null
"$BIN" --home $H/ws audit verify
install -d -m 0700 $H/sd
"$BIN" signer serve --dir $H/sd --socket $H/sock >$H/signer.log 2>&1 & SP=$!
for _ in $(seq 50); do [ -S $H/sock ] && break; sleep 0.1; done
# key 1 was auto-created by serve; restart after import so key 0 is loaded
"$BIN" --home $H/ws audit migrate-to-signer --signer-dir $H/sd --socket $H/sock && { echo "FAIL: verified without key 0 loaded"; exit 1; } || echo "ok: refused until signer reloads"
kill $SP; wait $SP 2>/dev/null || true
"$BIN" signer serve --dir $H/sd --socket $H/sock >$H/signer.log 2>&1 & SP=$!
for _ in $(seq 50); do [ -S $H/sock ] && break; sleep 0.1; done
"$BIN" --home $H/ws audit migrate-to-signer --signer-dir $H/sd --socket $H/sock
[ ! -e $H/secrets/audit.key ] && echo "ok: app-side key deleted"
export CMSQ_AUDIT_SIGNER=$H/sock
"$BIN" --home $H/ws audit verify
"$BIN" --home $H/ws init >/dev/null 2>&1 || true   # appends entries through the signer
"$BIN" --home $H/ws audit verify
echo "--- un-enrolled workspace is refused (no TOFU):"
if "$BIN" --home $H/ws2 init >/dev/null 2>&1; then echo "FAIL: TOFU"; exit 1; else echo "ok: refused"; fi
"$BIN" --home $H/ws2 audit signer-enroll --signer-dir $H/sd
"$BIN" --home $H/ws2 init >/dev/null && "$BIN" --home $H/ws2 audit verify
echo "SIGNER MIGRATE E2E PASSED"
