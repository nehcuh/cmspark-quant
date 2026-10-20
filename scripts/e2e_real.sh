#!/usr/bin/env bash
# Real-data end-to-end: tushare (primary, adj_factor) cross-validated against
# akshare/baostock → Docker sandbox backtests of the sample posts on real A-shares
# → host scoring + gate → typed promote / one-shot vault for anything that passes.
# Exits nonzero on infrastructure errors or on a gate pass with unvalidated data.
# Results are printed as they are — nothing here is tuned to make a strategy pass.
set -uo pipefail
cd "$(dirname "$0")/.."
SYMS=${SYMS:-"SH600519 SZ000001 SH601318 SZ000858 SH600036 SZ300750"}
COUNT=${COUNT:-2500}
export CMSQ_HOME=${CMSQ_HOME:-$(mktemp -d /tmp/cmsq-real.XXXX)}
export CMSQ_SECRETS_DIR=${CMSQ_SECRETS_DIR:-$CMSQ_HOME.secrets}
cargo build -q -p cmsq-cli || exit 2
BIN=target/debug/cmsq
FAIL=0; bad() { echo "!! $*"; FAIL=1; }
hr() { printf '\n== %s ==\n' "$*"; }
docker info >/dev/null 2>&1 || { echo "FAIL: docker not reachable (README: Docker setup)"; exit 2; }

hr "init + trade calendar + real data (host-side)"
$BIN init >/dev/null
$BIN data calendar || bad "trade calendar fetch failed"
OK_SYMS=()
for s in $SYMS; do
  if $BIN data fetch "$s" --count "$COUNT"; then OK_SYMS+=("$s"); else bad "fetch $s failed"; fi
done
$BIN data sources
[ ${#OK_SYMS[@]} -gt 0 ] || { echo "no real data"; exit 2; }

hr "ingest + extract sample posts"
cp fixtures/posts/*.json "$CMSQ_HOME/drop/"
$BIN ingest >/dev/null && $BIN extract >/dev/null
IDS=$($BIN list --json | python3 -c "import json,sys; print(' '.join(s['id'][:8] for s in json.load(sys.stdin) if s['status']=='Extracted'))")

hr "backtests on real data (every post × every symbol; each run counts as a library trial)"
for s in "${OK_SYMS[@]}"; do
  echo "-- native reference replay (not gated):"; $BIN replay "$s" --strategy buy_and_hold | head -2
  for id in $IDS; do
    st=$($BIN show "$id" --json | python3 -c "import json,sys; print(json.load(sys.stdin)['strategy']['status'])")
    case $st in Extracted|Rejected|Failed) ;; *) continue;; esac
    $BIN backtest "$id" --symbol "$s" --backend docker 2>/dev/null | head -4 || bad "backtest $id $s crashed"
    st=$($BIN show "$id" --json | python3 -c "import json,sys; print(json.load(sys.stdin)['strategy']['status'])")
    if [ "$st" = GatePassed ]; then
      python3 - "$CMSQ_HOME/data/$s.meta.json" <<'P' || bad "$id passed the gate on data that is not cross-validated"
import json,sys; v=json.load(open(sys.argv[1]))['extra'].get('validation',{}); sys.exit(0 if v.get('status')=='ok' else 1)
P
      echo "-- $id GatePassed on $s: running the one-shot vault"
      $BIN promote "$id" --confirm "PROMOTE $id" || echo "   (vault decided: not promoted)"
    fi
  done
done

hr "results"
$BIN list
$BIN trials | tail -5
$BIN audit verify || bad "audit verify failed"
echo; [ $FAIL = 0 ] && echo "E2E-REAL OK — workspace $CMSQ_HOME" || { echo "E2E-REAL FAILED"; exit 1; }
