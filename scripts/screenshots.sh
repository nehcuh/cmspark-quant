#!/usr/bin/env bash
# Retake tab-level dashboard screenshots on an X display (DISPLAY must be set).
#   REAL_HOME: a real-data workspace;  DEMO_HOME: the DEMO workspace from scripts/paper_demo.sh
set -euo pipefail
cd "$(dirname "$0")/.."
DASH=${DASH:-target/release/cmsq-dashboard}
OUT=docs/screenshots
shot() { # home secrets db tab file [run]
  CMSQ_HOME=$1 CMSQ_SECRETS_DIR=$2 CMSQ_DB=$3 CMSQ_DASH_TAB=$4 CMSQ_DASH_RUN=${6:-} "$DASH" >/tmp/dash.log 2>&1 & local p=$!
  for _ in $(seq 60); do xdotool search --name "CMspark Quant Dashboard" >/dev/null 2>&1 && break; sleep 0.5; done
  sleep 4
  local w; w=$(xdotool search --name "CMspark Quant Dashboard" | head -1)
  xdotool windowactivate "$w" 2>/dev/null || true; sleep 1
  import -window "$w" "$OUT/$5"
  kill $p; wait $p 2>/dev/null || true
  echo "$OUT/$5"
}
R=${REAL_HOME:?}; RS=${REAL_SECRETS:-$R.secrets}; D=${DEMO_HOME:-/tmp/cmsq-paper-demo}
shot "$R" "$RS" sqlite overview 01-overview.png
shot "$R" "$RS" sqlite library  03-library.png
shot "$R" "$RS" sqlite compare  10-compare.png
shot "$R" "$RS" sqlite jobs     11-jobs.png
shot "$R" "$RS" sqlite audit    12-audit.png
shot "$R" "$RS" sqlite data     14-market-data.png
shot "$R" "$RS" sqlite research 15-sweep.png sweep
shot "$R" "$RS" sqlite research 16-portfolio.png portfolio
shot "$R" "$RS" sqlite futures  17-futures-ctp.png
shot "$R" "$RS" sqlite signals  19-signals-real.png
shot "$D" "$D.secrets" sqlite signals 20-signals-demo.png
shot "$D" "$D.secrets" sqlite paper   21-paper-demo.png
shot "$D" "$D.secrets" sqlite overview 22-demo-banner.png
