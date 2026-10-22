#!/usr/bin/env bash
# Regenerate every dashboard screenshot and a before/after review sheet (docs/UI_DESIGN.md §5).
#   REAL_HOME  real-data workspace (sqlite)            -> tab shots (scripts/screenshots.sh)
#   DEMO_HOME  DEMO workspace (marker file DEMO)       -> paper/signals/DEMO banner shots
#   SHOTS_HOME, SCEN_DEMO_HOME, V7_HOME (optional) -> interaction shots (scripts/screenshots_scenarios.sh)
# Needs DISPLAY, xdotool, ImageMagick; run under `sg docker` so the no-Docker banner is not shown.
# Never points at the Tushare secrets file: the dashboard only reads the given workspaces.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${DISPLAY:?set DISPLAY}"; : "${REAL_HOME:?}"; : "${DEMO_HOME:?}"
[ -f "$DEMO_HOME/DEMO" ] || { echo "refusing: $DEMO_HOME has no DEMO marker"; exit 2; }
cargo build -q --release -p cmsq-dashboard
cargo build -q -p cmsq-cli
export DASH=target/release/cmsq-dashboard
scripts/screenshots.sh
if [ -n "${SHOTS_HOME:-}" ]; then DEMO_HOME=${SCEN_DEMO_HOME:?SCEN_DEMO_HOME: DEMO workspace for the scenario shots} scripts/screenshots_scenarios.sh; fi
# before/after review sheet for every image kept in docs/screenshots/before/
OUT=${REVIEW_DIR:-/tmp/cmsq-ui-review}; mkdir -p "$OUT"
for b in docs/screenshots/before/*.png; do
  n=$(basename "$b"); a=docs/screenshots/$n; [ -f "$a" ] || continue
  montage -label 'before %f' "$b" -label 'after %f' "$a" -tile 2x1 -geometry +8+8 -background '#222' -fill white "$OUT/compare-$n"
  echo "$OUT/compare-$n"
done
echo "review each sheet against docs/UI_DESIGN.md §5 before committing"
