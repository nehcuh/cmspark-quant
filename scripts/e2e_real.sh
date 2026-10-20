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
FUTS=${FUTS:-"FUT_RB"}
# akshare/baostock (cross-validation) live in a separate venv; default to ../.venv-data if present
[ -z "${CMSQ_PYTHON:-}" ] && [ -x ../.venv-data/bin/python ] && export CMSQ_PYTHON=$(cd .. && pwd)/.venv-data/bin/python
REAL_SECRETS=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
export CMSQ_HOME=${CMSQ_HOME:-$(mktemp -d /tmp/cmsq-real.XXXX)}
export CMSQ_SECRETS_DIR=$CMSQ_HOME.secrets
mkdir -p "$CMSQ_SECRETS_DIR" && chmod 700 "$CMSQ_SECRETS_DIR"
# the tushare token stays in the real secrets dir; only the env var is passed on
[ -s "$REAL_SECRETS/secrets.env" ] && { set -a; . "$REAL_SECRETS/secrets.env"; set +a; }
if [ "${CMSQ_DB:-postgres}" != sqlite ]; then
  [ -s "$REAL_SECRETS/db.env" ] || CMSQ_SECRETS_DIR=$REAL_SECRETS scripts/db_up.sh
  install -m 600 "$REAL_SECRETS/db.env" "$CMSQ_SECRETS_DIR/db.env"
fi
cargo build -q -p cmsq-cli || exit 2
BIN=target/debug/cmsq
FAIL=0; bad() { echo "!! $*"; FAIL=1; }
hr() { printf '\n== %s ==\n' "$*"; }
docker info >/dev/null 2>&1 || { echo "FAIL: docker not reachable (README: Docker setup)"; exit 2; }

hr "init + trade calendar + real data (host-side)"
$BIN init >/dev/null
$BIN data calendar || bad "trade calendar fetch failed"
OK_SYMS=(); OK_FUTS=()
for s in $SYMS; do
  if $BIN data fetch "$s" --count "$COUNT"; then OK_SYMS+=("$s"); else bad "fetch $s failed"; fi
done
for f in $FUTS; do
  $BIN data fetch "$f" --count "${FUT_COUNT:-1100}" && OK_FUTS+=("$f") || bad "fetch $f failed"
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

hr "P1: parameter sweep (every point counts in the DSR ledger) + portfolio on real A-shares"
MA=$($BIN list --json | python3 -c "import json,sys; v=[s['id'][:8] for s in json.load(sys.stdin) if s['title'].startswith('双均线')]; print(v[0] if v else '')")
$BIN sweep "$MA" --symbol "${OK_SYMS[0]}" --grid '{"fast":[5,10,20],"slow":[30,60]}' --workers 4 --backend docker | grep -v "^  ·" || bad "sweep failed"
$BIN portfolio "$MA" --symbols "$(IFS=,; echo "${OK_SYMS[*]}")" --alloc inverse_vol:60 --workers 4 --backend docker | grep -v "^  ·" || bad "portfolio failed"

hr "P2: China futures (main-contract roll, multiplier/margin/limits/fees) incl. a vn.py CtaTemplate post"
for f in "${OK_FUTS[@]}"; do
  python3 -c "import json,sys; m=json.load(open(sys.argv[1])); e=m['extra']; print('  ', m['symbol'] if 'symbol' in m else '', 'rolls', len(e.get('rolls',[])), 'validation', e.get('validation',{}).get('status'), e.get('validation',{}).get('detail','')[:120])" "$CMSQ_HOME/data/$f.meta.json" || true
  for t in "期货唐奇安" "vn.py 双均线"; do
    id=$($BIN list --json | python3 -c "import json,sys; v=[s['id'][:8] for s in json.load(sys.stdin) if s['title'].startswith(sys.argv[1])]; print(v[0] if v else '')" "$t")
    [ -n "$id" ] || { bad "no post $t"; continue; }
    $BIN backtest "$id" --symbol "$f" --backend docker 2>/dev/null | head -4 || bad "futures backtest $id $f crashed"
  done
done

hr "results"
$BIN list
$BIN trials | tail -5
$BIN audit verify || bad "audit verify failed"
echo; [ $FAIL = 0 ] && echo "E2E-REAL OK — workspace $CMSQ_HOME" || { echo "E2E-REAL FAILED"; exit 1; }
