#!/usr/bin/env bash
# End-to-end: drop → ingest → extract → sandbox backtest → gate → promote/reject → audit verify.
# Uses a fresh throw-away workspace. Real Docker sandbox when available (else DEV bwrap backend).
set -euo pipefail
cd "$(dirname "$0")/.."
export CMSQ_HOME="${CMSQ_HOME:-$(mktemp -d /tmp/cmsq-e2e.XXXX)}"
BIN=target/debug/cmsq
hr() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

hr "build"
cargo build -q -p cmsq-cli -j4
BACKEND=process
if docker info >/dev/null 2>&1; then
  BACKEND=docker
  docker image inspect cmsq-runner:1.0.0 >/dev/null 2>&1 || scripts/build_runner.sh
else
  echo "WARNING: docker not usable by $(id -un); using DEV process backend (results cannot pass the gate)"
fi
echo "workspace: $CMSQ_HOME   backend: $BACKEND"

hr "init + market data (host-side; sandbox stays offline)"
$BIN init
if $BIN data fetch SH600519 --count 1200 2>/tmp/cmsq-tdx.err; then
  REAL=SH600519
else
  echo "rstdx/TDX fetch failed: $(tail -1 /tmp/cmsq-tdx.err) — using bundled SAMPLE data (offline fallback)"
  REAL=""
fi
$BIN data list

hr "1. CMspark drop → ingest"
cp fixtures/posts/*.json "$CMSQ_HOME/drop/"
echo '{"spec":"cmspark.scrape.v1","source":{"site":"x"}}' > "$CMSQ_HOME/drop/broken.json"
$BIN ingest
echo "-- re-dropping one file (dedupe):"; cp fixtures/posts/ma_crossover.json "$CMSQ_HOME/drop/again.json"; $BIN ingest

hr "2. extract (offline heuristic extractor)"
$BIN extract

hr "3+4. sandbox backtests (trend-style ideas on SAMPLE_TREND)"
ids() { $BIN list --json | python3 -c "import json,sys; t=sys.argv[1]; print(' '.join(s['id'][:8] for s in json.load(sys.stdin) if s['status']=='Extracted' and t in s['title']))" "$1"; }
for t in 双均线 动量 明日 网格; do for id in $(ids "$t"); do $BIN backtest "$id" --symbol SAMPLE_TREND --backend $BACKEND; done; done
hr "3+4. mean-reversion ideas on SAMPLE_RANGE"
for t in 布林 短期反转; do for id in $(ids "$t"); do $BIN backtest "$id" --symbol SAMPLE_RANGE --backend $BACKEND; done; done
if [ -n "$REAL" ]; then
  hr "real A-share data via rstdx: re-run MA crossover on $REAL"
  id=$($BIN list --json | python3 -c "import json,sys; print([s['id'][:8] for s in json.load(sys.stdin) if '双均线' in s['title']][0])")
  $BIN backtest "$id" --symbol "$REAL" --backend $BACKEND
fi

hr "5. manual promote (confirmation desk)"
PASS=$($BIN list --status Evaluated --json | python3 -c "import json,sys; v=json.load(sys.stdin); print(v[0]['id'][:8] if v else '')")
if [ -n "$PASS" ]; then
  echo "-- wrong confirmation is refused:"; $BIN promote "$PASS" --confirm nope || true
  $BIN promote "$PASS" --confirm "$PASS"
else
  echo "no strategy passed the gate"
fi

hr "library"
$BIN list
hr "jobs"
$BIN jobs
hr "signals"
$BIN signals
hr "audit"
$BIN audit verify
cp "$CMSQ_HOME/cmsq.db" /tmp/cmsq-tamper.db
python3 - <<'PY'
import sqlite3; c=sqlite3.connect("/tmp/cmsq-tamper.db")
c.executescript("DROP TRIGGER audit_no_update;"); c.execute("UPDATE audit SET actor='attacker' WHERE seq=3"); c.commit()
PY
echo "-- tampered copy:"; CMSQ_HOME=/tmp/cmsq-tamper-home $BIN init >/dev/null; cp /tmp/cmsq-tamper.db /tmp/cmsq-tamper-home/cmsq.db
CMSQ_HOME=/tmp/cmsq-tamper-home $BIN audit verify && { echo "TAMPER NOT DETECTED"; exit 1; } || echo "(tamper detected as expected)"
rm -rf /tmp/cmsq-tamper-home /tmp/cmsq-tamper.db
hr "done — workspace kept at $CMSQ_HOME"
