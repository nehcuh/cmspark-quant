#!/usr/bin/env bash
# DEMO ONLY — paper trading that follows a Promoted strategy over (simulated) time.
# Separate workspace (default /tmp/cmsq-paper-demo, marked by a DEMO file -> CLI/dashboard
# banners). The strategy is validated on SYNTHETIC data (SAMPLE_DEMO = SAMPLE_PERSIST extended through 2020, DSR trial floor 1,
# CMSQ_ALLOW_SYNTHETIC=1); "time passes" by revealing more of the synthetic series
# (CMSQ_SAMPLE_AS_OF). Quotes are the synthetic daily bars. NOT evidence of any edge.
# Never point this at a real workspace.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-$PWD/target/debug/cmsq}
D=${DEMO_HOME:-/tmp/cmsq-paper-demo}
case "$D" in *demo*) ;; *) echo "refusing: DEMO_HOME must contain 'demo'"; exit 2;; esac
rm -rf "$D" "$D.secrets"; mkdir -p "$D" "$D.secrets"; chmod 700 "$D.secrets"
echo "DEMO: synthetic-validated strategy, simulated time, NOT evidence" > "$D/DEMO"
[ -f "$HOME/.config/cmsq/runner_image_id" ] && install -m 600 "$HOME/.config/cmsq/runner_image_id" "$D.secrets/"
export CMSQ_HOME=$D CMSQ_SECRETS_DIR=$D.secrets CMSQ_DB=sqlite CMSQ_DSR_MIN_TRIALS=1 CMSQ_ALLOW_SYNTHETIC=1
unset CMSQ_AUDIT_SIGNER CMSQ_DATABASE_URL
SYM=SAMPLE_DEMO; START=2019-12-31
CMSQ_SAMPLE_AS_OF=$START "$BIN" init >/dev/null 2>&1
CMSQ_SAMPLE_AS_OF=$START "$BIN" data fetch $SYM >/dev/null 2>&1
cp fixtures/posts/vnpy_double_ma.json "$D/drop/"; "$BIN" ingest >/dev/null; "$BIN" extract >/dev/null
ID=$("$BIN" list --json | python3 -c "import json,sys; print([s['id'][:8] for s in json.load(sys.stdin) if s['status']=='Extracted'][0])")
"$BIN" backtest "$ID" --symbol $SYM --backend docker 2>&1 | tail -1
"$BIN" promote "$ID" --confirm "PROMOTE $ID" | tail -1
"$BIN" show "$ID" --json | python3 -c "import json,sys; s=json.load(sys.stdin)['strategy']['status']; assert s=='Promoted', s"
# simulated time: reveal one more month at a time, refresh signals (sandbox + probes)
for m in 2020-01-31 2020-02-29 2020-03-31 2020-04-30 2020-05-31 2020-06-30 2020-07-31 2020-08-31 2020-09-30 2020-10-31 2020-11-30 2020-12-31; do
  CMSQ_SAMPLE_AS_OF=$m "$BIN" signals --refresh --fetch --backend docker 2>&1 | tail -1
done
# quotes = the synthetic daily bars of the new period (one close-time bar per day)
python3 - "$D" <<'PY'
import sys,csv
d=sys.argv[1]; rows=list(csv.reader(open("fixtures/market/SAMPLE_DEMO.csv")))
out=open(f"{d}/demo_quotes.csv","w"); out.write("datetime,open,high,low,close,volume\n")
for r in rows[1:]:
    if "2020-01-01" <= r[0] <= "2020-12-31": out.write(f"{r[0]} 14:59:00,{r[1]},{r[2]},{r[3]},{r[4]},{r[5]}\n")
PY
"$BIN" paper run --strategy "$ID" --symbol $SYM --from-bars "$D/demo_quotes.csv" --session demo_paper_$ID | tail -2
"$BIN" signals --id "$ID" | head -5
echo "DEMO finished — workspace $D (banners shown in CLI and dashboard). NOT evidence."
