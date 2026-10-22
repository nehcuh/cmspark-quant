#!/usr/bin/env bash
# OPTIONAL (heavy, not in CI): real-data Kronos run through the hardened Docker
# sandbox (frozen clock, seccomp, no network). Needs scripts/build_runner_kronos.sh
# first, docker access, Postgres (scripts/db_up.sh) and the tushare token.
# Uses a throwaway workspace; your cmsq.env is not touched.
set -euo pipefail
cd "$(dirname "$0")/.."
[ -z "${CMSQ_PYTHON:-}" ] && [ -x ../.venv-data/bin/python ] && export CMSQ_PYTHON=$(cd .. && pwd)/.venv-data/bin/python
R=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
export CMSQ_HOME=$(mktemp -d /tmp/cmsq-kronos.XXXX)
export CMSQ_SECRETS_DIR=$CMSQ_HOME.secrets
mkdir -p "$CMSQ_SECRETS_DIR" && chmod 700 "$CMSQ_SECRETS_DIR"
for f in db.env secrets.env runner_image_id runner_kronos_image_id; do
  [ -f "$R/$f" ] && install -m 600 "$R/$f" "$CMSQ_SECRETS_DIR/$f"
done
[ -s "$CMSQ_SECRETS_DIR/runner_kronos_image_id" ] || { echo "run scripts/build_runner_kronos.sh first"; exit 2; }
cargo build -q -p cmsq-cli
BIN=target/debug/cmsq
SYM=${1:-SH600519}
$BIN init >/dev/null
$BIN data calendar >/dev/null
$BIN data fetch "$SYM" --count "${COUNT:-420}"
cp examples/posts/kronos_direction.json "$CMSQ_HOME/drop/"
$BIN ingest >/dev/null && $BIN extract >/dev/null
ID=$($BIN list --json | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['id'][:8])")
$BIN backtest "$ID" --symbol "$SYM" --backend docker
echo "workspace: $CMSQ_HOME"
