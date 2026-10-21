#!/usr/bin/env bash
# Prepare a demo workspace (strategies backtested in Docker, one GatePassed waiting for
# the typed confirmation + vault, one fresh drop waiting) and launch the dashboard.
set -euo pipefail
cd "$(dirname "$0")/.."
export CMSQ_HOME="${CMSQ_HOME:-/tmp/cmsq-demo}"
export CMSQ_SECRETS_DIR="$CMSQ_HOME.secrets"   # always derived: never wipe an inherited secrets dir
rm -rf "$CMSQ_HOME" "$CMSQ_SECRETS_DIR"
cargo build -q -p cmsq-cli -p cmsq-dashboard
B=target/debug/cmsq
$B init >/dev/null
for f in ma_crossover momentum_20d mean_reversion_z future_peek phone_home; do cp fixtures/posts/$f.json "$CMSQ_HOME/drop/"; done
cp fixtures/attacks/hidden_peek.json fixtures/attacks/forge.json "$CMSQ_HOME/drop/"
$B ingest && $B extract >/dev/null
id() { $B list --json | python3 -c "import json,sys; print([s['id'][:8] for s in json.load(sys.stdin) if sys.argv[1] in s['title']][0])" "$1"; }
$B backtest "$(id 双均线)" --symbol SAMPLE_REGIME 2>/dev/null || true
$B backtest "$(id 20日动量)" --symbol SAMPLE_REGIME 2>/dev/null || true
$B backtest "$(id 布林带)" --symbol SAMPLE_RANGE 2>/dev/null || true
for t in 明日涨跌 网格 ADV_hidden_peek ADV_forge; do $B backtest "$(id $t)" --symbol SAMPLE_TREND 2>/dev/null || true; done
cp fixtures/posts/short_term_reversal.json "$CMSQ_HOME/drop/"   # left for the UI "Ingest" button
[ -n "${PREP_ONLY:-}" ] && exit 0
exec target/debug/cmsq-dashboard
