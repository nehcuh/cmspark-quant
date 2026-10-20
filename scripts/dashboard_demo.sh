#!/usr/bin/env bash
# Prepare a demo workspace (5 strategies already backtested, 1 fresh drop waiting) and launch the dashboard.
set -euo pipefail
cd "$(dirname "$0")/.."
export CMSQ_HOME="${CMSQ_HOME:-/tmp/cmsq-demo}"
rm -rf "$CMSQ_HOME"
cargo build -q -p cmsq-cli -p cmsq-dashboard -j3
B=target/debug/cmsq
$B init >/dev/null
for f in ma_crossover momentum_20d mean_reversion_z future_peek phone_home; do cp fixtures/posts/$f.json "$CMSQ_HOME/drop/"; done
$B ingest && $B extract >/dev/null && $B backtest --all --symbol SAMPLE_TREND >/dev/null
cp fixtures/posts/short_term_reversal.json "$CMSQ_HOME/drop/"   # left for the UI "Ingest" button
exec target/debug/cmsq-dashboard
