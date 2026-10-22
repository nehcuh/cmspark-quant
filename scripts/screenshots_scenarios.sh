#!/usr/bin/env bash
# Retake the interaction-state screenshots (02, 04-06, 08, 09, 13, 18, 23).
# Scroll: wheel clicks with the pointer on the detail pane's right edge (x=1016), so nested
# code/log scroll areas never swallow the wheel. Re-check counts after layout changes.
# Needs DISPLAY, Docker (run under `sg docker`), a release dashboard and a debug cli.
#   SHOTS_HOME : sample workspace from `PREP_ONLY=1 scripts/dashboard_demo.sh`
#   REAL_HOME  : real-data workspace (sqlite) with a FUT_RB sweep
#   DEMO_HOME  : DEMO workspace (marker file DEMO) with a GatePassed strategy; the
#                script promotes it there (synthetic data, NOT evidence)
set -euo pipefail
cd "$(dirname "$0")/.."
DASH=${DASH:-target/release/cmsq-dashboard}; OUT=docs/screenshots
S=${SHOTS_HOME:?}; R=${REAL_HOME:?}; D=${DEMO_HOME:?}
[ -f "$D/DEMO" ] || { echo "refusing: $D has no DEMO marker"; exit 2; }
shot() { # home tab file select scroll [env...]
  if timeout 2 xdotool search --name "CMspark Quant Dashboard" >/dev/null 2>&1; then echo "refusing: another dashboard window is open (would be captured instead)"; return 1; fi
  local h=$1 t=$2 f=$3 sel=$4 sc=$5; shift 5
  env "$@" CMSQ_HOME="$h" CMSQ_SECRETS_DIR="$h.secrets" CMSQ_DB=sqlite CMSQ_DASH_TAB="$t" CMSQ_DASH_SELECT="$sel" "$DASH" >/tmp/dash.log 2>&1 & local p=$!
  for _ in $(seq 60); do xdotool search --name "CMspark Quant Dashboard" >/dev/null 2>&1 && break; sleep 0.5; done
  sleep 4; local w; w=$(timeout 5 xdotool search --name "CMspark Quant Dashboard" | head -1)
  [ -n "$w" ] || { echo "dashboard window not found (see /tmp/dash.log)"; kill $p; return 1; }
  xdotool windowactivate "$w" 2>/dev/null || true; sleep 1
  if [ "$sc" -gt 0 ]; then xdotool mousemove --window "$w" 1016 300; for _ in $(seq "$sc"); do xdotool click 5; sleep 0.05; done; sleep 1; fi
  xdotool mousemove --window "$w" 5 630 2>/dev/null || true; sleep 0.5  # park the pointer: no chart tooltip / hover state
  import -window "$w" "$OUT/$f"; kill $p; wait $p 2>/dev/null || true; echo "$OUT/$f"
}
shot "$S" library 02-forge-attack-rejected.png ADV_forge 0 X=1
shot "$S" library 04-gate-checks-equity.png 双均线 25 X=1
shot "$S" library 09-hidden-peek-caught.png ADV_hidden_peek 17 X=1
shot "$S" library 13-no-docker-banner.png ADV_forge 0 DOCKER_HOST=unix:///nonexistent.sock
shot "$R" research 18-futures-sweep-vnpy.png x 0 CMSQ_DASH_RUN=sweep
shot "$D" library 05-typed-confirmation.png vn.py 20 CMSQ_DASH_CONFIRM=1
ID=$(CMSQ_HOME=$D CMSQ_SECRETS_DIR=$D.secrets CMSQ_DB=sqlite target/debug/cmsq list | grep -m1 GatePassed | cut -c1-8 || true)
if [ -n "$ID" ]; then
  CMSQ_HOME=$D CMSQ_SECRETS_DIR=$D.secrets CMSQ_DB=sqlite CMSQ_DSR_MIN_TRIALS=1 CMSQ_ALLOW_SYNTHETIC=1 \
    target/debug/cmsq promote "$ID" --confirm "PROMOTE $ID" | tail -1
fi
shot "$D" jobs    06-vault-job-log.png vn.py 0 X=1
shot "$D" library 23-vault-passed-timeline.png vn.py 13 X=1
shot "$D" library 08-equity-with-vault.png vn.py 26 X=1
# 07: vault rejection, in its own DEMO workspace V7_HOME (ma_crossover on SAMPLE_REGIME
# passes dev with CMSQ_DSR_MIN_TRIALS=1, then fails the holdout: -10.50%)
if [ -n "${V7_HOME:-}" ]; then
  [ -f "$V7_HOME/DEMO" ] || { echo "refusing: $V7_HOME has no DEMO marker"; exit 2; }
  shot "$V7_HOME" library 07-vault-rejected-timeline.png 双均线 9 X=1
fi
