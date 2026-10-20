#!/usr/bin/env bash
# End-to-end: drop → ingest → extract → per-variant sandbox runs → trusted host
# scoring → gate → typed promote → one-time vault → audit verify + tamper attacks.
#
# Exits NON-ZERO when any strategy ends in an unexpected state, when an attack is
# not caught, when tampering is not detected, or when Docker is unavailable
# (pass --allow-dev to run on the DEV bwrap backend; then nothing may pass).
set -euo pipefail
cd "$(dirname "$0")/.."
ALLOW_DEV=0; [ "${1:-}" = "--allow-dev" ] && ALLOW_DEV=1
if [ -n "${CMSQ_HOME:-}" ] && [ -n "$(ls -A "$CMSQ_HOME" 2>/dev/null)" ]; then
  echo "refusing to run in a non-empty inherited CMSQ_HOME=$CMSQ_HOME (unset it, or point it at an empty dir)"; exit 2
fi
REAL_SECRETS=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
export CMSQ_HOME="${CMSQ_HOME:-$(mktemp -d /tmp/cmsq-e2e.XXXX)}"
export CMSQ_SECRETS_DIR="${CMSQ_SECRETS_DIR:-$CMSQ_HOME.secrets}"   # outside CMSQ_HOME
BIN=target/debug/cmsq
FAILS=0
hr() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
bad() { echo "E2E FAILURE: $*"; FAILS=$((FAILS+1)); }

hr "build"
cargo build -q -p cmsq-cli
if docker info >/dev/null 2>&1; then
  BACKEND=docker
  docker image inspect cmsq-runner:2.2.0 >/dev/null 2>&1 || scripts/build_runner.sh
elif [ $ALLOW_DEV = 1 ]; then
  BACKEND=process; echo "!!! DEV backend (--allow-dev): nothing may pass the gate"
else
  echo "docker not usable by $(id -un) — see README 'Docker setup' (or run with --allow-dev)"; exit 2
fi
# database: PostgreSQL/TimescaleDB (independent deployment) unless CMSQ_DB=sqlite
mkdir -p "$CMSQ_SECRETS_DIR" && chmod 700 "$CMSQ_SECRETS_DIR"
if [ "${CMSQ_DB:-postgres}" != sqlite ]; then
  [ -s "$REAL_SECRETS/db.env" ] || CMSQ_SECRETS_DIR=$REAL_SECRETS scripts/db_up.sh
  install -m 600 "$REAL_SECRETS/db.env" "$CMSQ_SECRETS_DIR/db.env"   # each run gets its own schema ws_<hash(home)>
  DBK=postgres
else
  DBK=sqlite
fi
echo "workspace: $CMSQ_HOME   secrets: $CMSQ_SECRETS_DIR   backend: $BACKEND   db: $DBK"

hr "init + market data (host-side; containers never see the dataset)"
$BIN init
if $BIN data fetch SH600519 --count 1200 2>/tmp/cmsq-tdx.err; then
  echo "real data cached via the multi-source router (see adjustment + cross-validation above)"
else
  echo "real-data fetch failed: $(tail -1 /tmp/cmsq-tdx.err) — bundled SAMPLE data only"
fi
$BIN data list

hr "1. CMspark drop → ingest (posts + reviewer attack posts)"
cp fixtures/posts/*.json fixtures/attacks/*.json "$CMSQ_HOME/drop/"
echo '{"spec":"cmspark.scrape.v1","source":{"site":"x"}}' > "$CMSQ_HOME/drop/broken.json"
ln -s /etc/passwd "$CMSQ_HOME/drop/symlink_attack.json"
$BIN ingest
for f in broken evil_deep symlink_attack; do [ -e "$CMSQ_HOME/drop/rejected/$f.json" ] || [ -L "$CMSQ_HOME/drop/rejected/$f.json" ] || bad "$f.json was not rejected at ingest"; done
echo "-- re-dropping one file (dedupe):"; cp fixtures/posts/ma_crossover.json "$CMSQ_HOME/drop/again.json"; $BIN ingest

hr "2. extract (offline heuristic extractor)"
$BIN extract

idof() { $BIN list --json | python3 -c "import json,sys; v=[s['id'][:8] for s in json.load(sys.stdin) if sys.argv[1] in s['title']]; print(v[0] if v else '')" "$1"; }
statusof() { $BIN show "$1" --json | python3 -c "import json,sys; print(json.load(sys.stdin)['strategy']['status'])"; }
# title-substring | symbol | allowed final statuses after backtest
EXPECT=(
  "双均线|SAMPLE_REGIME|GatePassed|Rejected"
  "20日动量|SAMPLE_REGIME|Rejected"
  "布林带|SAMPLE_RANGE|Rejected"
  "短期反转|SAMPLE_RANGE|Rejected"
  "明日涨跌|SAMPLE_TREND|Rejected"
  "网格|SAMPLE_TREND|Failed|Rejected"
  "ADV_forge|SAMPLE_TREND|Rejected"
  "ADV_memo_peek|SAMPLE_TREND|Rejected"
  "ADV_data_peek|SAMPLE_TREND|Failed"
  "ADV_hidden_peek|SAMPLE_TREND|Rejected"
  "ADV_length_tell|SAMPLE_TREND|Rejected"
  "ADV_symlink_out|SAMPLE_TREND|Failed"
  "ADV_escape|SAMPLE_TREND|Rejected"
  "ADV_forkbomb|SAMPLE_TREND|Failed|Rejected"
  "ADV_bigout|SAMPLE_TREND|Failed"
  "ADV2_clock_dev|SAMPLE_TREND|Rejected|Failed"
  "ADV2_clock_dev_bh|SAMPLE_TREND|Rejected|Failed"
  "ADV2_clock_vault|SAMPLE_TREND|Rejected|Failed"
  "ADV2_clock_probe|SAMPLE_TREND|Rejected"
  "均线趋势 + 大波动过滤|SAMPLE_REGIME|Rejected"
  "ADV2_rng_alias|SAMPLE_TREND|Rejected"
  "ADV2_sparse_peek|SAMPLE_TREND|Rejected"
)
[ $BACKEND = process ] && EXPECT[0]="双均线|SAMPLE_REGIME|Rejected"
hr "3+4. backtests: one fresh container per probe variant, scored on the host"
for e in "${EXPECT[@]}"; do
  IFS='|' read -r t sym allowed <<<"$e"
  id=$(idof "$t"); [ -n "$id" ] || { bad "no strategy titled $t"; continue; }
  $BIN backtest "$id" --symbol "$sym" --backend $BACKEND 2>/dev/null || true
done
# evil_path (title exactly "T", hostile path-like fields) must be rejected at ingest or fail extraction
id=$($BIN list --json | python3 -c "import json,sys; v=[s['id'][:8] for s in json.load(sys.stdin) if s['title']=='T']; print(v[0] if v else '')")
[ -z "$id" ] || [ "$(statusof "$id")" = Failed ] || bad "evil_path did not end Failed"

hr "5. typed confirmation + one-time vault holdout"
# In the shared library above the DSR is deflated over every idea tried (N>=20 floor),
# so nothing reaches GatePassed. To exercise the vault path honestly we use a SEPARATE,
# single-idea library with the trial floor explicitly lowered (CMSQ_DSR_MIN_TRIALS=1,
# recorded in each evaluation's gate config). This is a mechanics demo, not a pass claim.
V_HOME=$(mktemp -d /tmp/cmsq-e2e-vault.XXXX)
V="env CMSQ_HOME=$V_HOME CMSQ_SECRETS_DIR=$V_HOME.secrets CMSQ_DSR_MIN_TRIALS=1 $BIN"
mkdir -p "$V_HOME.secrets" && chmod 700 "$V_HOME.secrets"; [ $DBK = postgres ] && install -m 600 "$REAL_SECRETS/db.env" "$V_HOME.secrets/db.env"
$V init >/dev/null; cp fixtures/posts/ma_crossover.json "$V_HOME/drop/"; $V ingest >/dev/null; $V extract >/dev/null
vid() { $V list --json | python3 -c "import json,sys; v=[s['id'][:8] for s in json.load(sys.stdin) if sys.argv[1] in s['title']]; print(v[0] if v else '')" "$1"; }
vst() { $V show "$1" --json | python3 -c "import json,sys; print(json.load(sys.stdin)['strategy']['status'])"; }
PASS=$(vid "双均线"); $V backtest "$PASS" --symbol SAMPLE_REGIME --backend $BACKEND 2>/dev/null | head -3 || true
if [ "$(vst "$PASS")" = GatePassed ]; then
  echo "-- id prefix alone is refused:"; $V promote "$PASS" --confirm "$PASS" && bad "promote accepted a non-typed confirmation" || true
  $V promote "$PASS" --confirm "PROMOTE $PASS" || echo "(vault decided: not promoted)"
  FINAL=$(vst "$PASS"); echo "final: $FINAL"
  case "$FINAL" in Promoted|Rejected) ;; *) bad "after vault, status $FINAL";; esac
  echo "-- the vault is one-shot:"; $V promote "$PASS" --confirm "PROMOTE $PASS" && bad "vault reused" || true
  echo "-- a re-post of the same idea (new title/text) gets no fresh vault:"
  python3 - fixtures/posts/ma_crossover.json > "$V_HOME/drop/repost.json" <<'PY'
import json,sys; d=json.load(open(sys.argv[1])); d["content"]["title"]="REPOST 双均线 v2"; d["content"]["body"]=d["content"]["body"].replace("思路","新思路"); print(json.dumps(d,ensure_ascii=False))
PY
  $V ingest >/dev/null; $V extract >/dev/null; R2=$(vid "REPOST 双均线")
  $V backtest "$R2" --symbol SAMPLE_REGIME --backend $BACKEND 2>/dev/null | head -1 || true
  if [ "$(vst "$R2")" = GatePassed ]; then
    { $V promote "$R2" --confirm "PROMOTE $R2" 2>&1 || true; } | tail -1; [ "$(vst "$R2")" = Rejected ] && echo "   (vault already spent for this idea — refused as expected)" || bad "repost got a fresh vault"
  else
    echo "   repost ended $(vst "$R2") before promote"
  fi
  $V audit verify || bad "vault workspace audit did not verify"
elif [ $BACKEND = docker ]; then
  bad "single-idea vault demo: ma_crossover did not reach GatePassed"
fi

hr "6. data tampering: edit a read-only snapshot / strip the sha from meta.json"
BB=$(idof "布林带"); SH=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['extra']['snapshot_sha256'])" "$CMSQ_HOME/data/SAMPLE_RANGE.meta.json")
SNAP="$CMSQ_HOME/data/snapshots/$SH.csv"; SNAPBAK=$(mktemp); cp "$SNAP" "$SNAPBAK"; chmod u+w "$SNAPBAK"
chmod u+w "$SNAP"; sed -i '2s/,[0-9.]*,/,99.0,/' "$SNAP"
OUT=$($BIN backtest "$BB" --symbol SAMPLE_RANGE --backend $BACKEND 2>&1 || true); echo "   $(echo "$OUT" | tail -1 | cut -c1-160)"
echo "$OUT" | grep -q "integrity" && echo "   (edited snapshot refused as expected)" || bad "edited snapshot accepted"
cp "$SNAPBAK" "$SNAP"; chmod 444 "$SNAP"
cp "$CMSQ_HOME/data/SAMPLE_RANGE.meta.json" "$CMSQ_HOME/meta.bak"
python3 -c "import json,sys; p=sys.argv[1]; m=json.load(open(p)); m['extra'].pop('snapshot_sha256'); json.dump(m,open(p,'w'))" "$CMSQ_HOME/data/SAMPLE_RANGE.meta.json"
OUT=$($BIN backtest "$BB" --symbol SAMPLE_RANGE --backend $BACKEND 2>&1 || true); echo "   $(echo "$OUT" | tail -1 | cut -c1-160)"
echo "$OUT" | grep -q "integrity" && echo "   (meta.json without snapshot sha refused as expected)" || bad "missing snapshot sha accepted"
cp "$CMSQ_HOME/meta.bak" "$CMSQ_HOME/data/SAMPLE_RANGE.meta.json"

hr "7. P1 parameter sweep (every point is a DSR trial) + concurrent multi-symbol backtest + portfolio"
MA=$(idof "双均线"); T0=$($BIN trials --json 2>/dev/null | python3 -c "import json,sys; print(len(json.load(sys.stdin)))" 2>/dev/null || echo 0)
$BIN sweep "$MA" --symbol SAMPLE_REGIME --grid '{"fast":[5,10],"slow":[30,60]}' --workers 3 --backend $BACKEND || bad "sweep failed"
T1=$($BIN trials --json 2>/dev/null | python3 -c "import json,sys; print(len(json.load(sys.stdin)))" 2>/dev/null || echo 0)
[ $((T1 - T0)) -ge 4 ] && echo "   (4 sweep points recorded in the trial ledger: $T0 → $T1)" || bad "sweep points not all in the trial ledger ($T0 → $T1)"
$BIN backtest "$MA" --symbols SAMPLE_REGIME,SAMPLE_RANGE,SAMPLE_TREND --workers 3 --backend $BACKEND > "$CMSQ_HOME/multi.txt" || bad "multi-symbol backtest failed"; grep -v "^  ·" "$CMSQ_HOME/multi.txt"
$BIN portfolio "$MA" --symbols SAMPLE_REGIME,SAMPLE_RANGE,SAMPLE_TREND --alloc inverse_vol:20 --workers 3 --backend $BACKEND || bad "portfolio failed"
$BIN runs

hr "8. P2 vn.py CtaTemplate post (shim inside the sandbox) on bundled A-share sample"
VN=$(idof "vn.py 双均线"); [ -n "$VN" ] || bad "vn.py post not extracted"
[ -n "$VN" ] && { $BIN show "$VN" --json | python3 -c "import json,sys; d=json.load(sys.stdin); print('   params', d['strategy'].get('extracted',{}).get('parameters'))" || true; }
[ -n "$VN" ] && { $BIN backtest "$VN" --symbol SAMPLE_TREND --backend $BACKEND > "$CMSQ_HOME/vn.txt" || bad "vn.py backtest failed"; grep -v "^  ·" "$CMSQ_HOME/vn.txt"; }
[ -n "$VN" ] && case "$(statusof "$VN")" in Rejected|GatePassed) ;; *) bad "vn.py strategy ended $(statusof "$VN") (runner shim not working?)";; esac

hr "9. P3 CTP market data (observe-only) — replay feed with malformed ticks"
$BIN md probe || echo "   (openctp front not reachable from here; replay feed used below)"
$BIN md run --instruments rb2601,au2612 --mock --seconds 20 || bad "md replay failed"
TK=$($BIN db status --json | python3 -c "import json,sys; print(json.load(sys.stdin)['ticks'])")
[ "$TK" = 362 ] && echo "   (362 trusted ticks stored, malformed ones rejected)" || bad "expected 362 stored ticks, got $TK"

hr "expected outcomes"
printf '%-18s %-14s %-12s %s\n' TITLE SYMBOL STATUS EXPECTED
for e in "${EXPECT[@]}"; do
  IFS='|' read -r t sym allowed <<<"$e"
  id=$(idof "$t"); st=$(statusof "$id")
  ok=FAIL; for a in ${allowed//|/ }; do [ "$st" = "$a" ] && ok=ok; done
  printf '%-18s %-14s %-12s %-22s %s\n' "$t" "$sym" "$st" "$allowed" "$ok"
  [ $ok = ok ] || bad "$t ended $st, expected $allowed"
done

hr "library / jobs / trials / signals"
$BIN list; $BIN jobs > "$CMSQ_HOME/jobs.txt"; head -20 "$CMSQ_HOME/jobs.txt"; $BIN trials > "$CMSQ_HOME/trials.txt"; head -3 "$CMSQ_HOME/trials.txt"; $BIN signals

hr "audit: verify, then reviewer tamper attacks on the live DB (restored after each)"
$BIN audit verify || bad "clean audit chain did not verify"
if [ $DBK = sqlite ]; then
DB="$CMSQ_HOME/cmsq.db"
attack() { # $1 = name, $2 = SQL/python body
  python3 - "$DB" "$2" <<'PY'
import sqlite3, sys, hashlib
db, kind = sys.argv[1], sys.argv[2]
c = sqlite3.connect(db); bk = sqlite3.connect(db + ".bak"); c.backup(bk); bk.close()
c.executescript("DROP TRIGGER IF EXISTS audit_no_update; DROP TRIGGER IF EXISTS audit_no_delete;")
if kind == "edit":
    c.execute("UPDATE audit SET actor='attacker' WHERE seq=3")
elif kind == "truncate":
    c.execute("DELETE FROM audit WHERE seq > (SELECT MAX(seq)-3 FROM audit)")
elif kind == "rechain":   # re-compute every link with the PUBLIC (unkeyed) algorithm
    prev = "0" * 64
    for seq, ts, actor, action, target, details, success in c.execute("SELECT seq,ts,actor,action,target,details,success FROM audit ORDER BY seq").fetchall():
        if seq == 3: actor = "attacker"
        h = hashlib.sha256()
        for p in [prev, str(seq), ts, actor, action, target or "", details, "1" if success else "0"]:
            b = p.encode(); h.update(len(b).to_bytes(8, "little")); h.update(b)
        hh = h.hexdigest(); c.execute("UPDATE audit SET actor=?, prev_hash=?, hash=? WHERE seq=?", (actor, prev, hh, seq)); prev = hh
c.commit()
PY
  if $BIN audit verify; then bad "audit attack '$1' NOT detected"; else echo "   ($1 detected as expected)"; fi
  python3 -c "import sqlite3,sys; s=sqlite3.connect(sys.argv[1]+'.bak'); d=sqlite3.connect(sys.argv[1]); s.backup(d)" "$DB"; rm -f "$DB.bak"
}
else
# PostgreSQL: the app role cannot touch the evidence tables at all (tests/store_concurrency.rs);
# here the attacker is the DB OWNER (superuser) who disables the triggers. The keyed chain and
# the external anchor must still catch it.
SCHEMA=$($BIN db status --json | python3 -c "import json,sys; print(json.load(sys.stdin)['schema'])")
PSQL="docker exec -i cmsq-db-1 psql -q -X -v ON_ERROR_STOP=1 -U cmsq_owner -d cmsq"
echo "   schema $SCHEMA"
attack() {
  echo "SET search_path=$SCHEMA; CREATE TABLE audit_bak AS SELECT * FROM audit;" | $PSQL
  case $2 in
    edit) echo "SET search_path=$SCHEMA; ALTER TABLE audit DISABLE TRIGGER USER; UPDATE audit SET actor='attacker' WHERE seq=3; ALTER TABLE audit ENABLE TRIGGER USER;" | $PSQL ;;
    truncate) echo "SET search_path=$SCHEMA; ALTER TABLE audit DISABLE TRIGGER USER; DELETE FROM audit WHERE seq > (SELECT MAX(seq)-3 FROM audit); ALTER TABLE audit ENABLE TRIGGER USER;" | $PSQL ;;
    rechain) $PSQL -At -F $'\x1f' -c "SET search_path=$SCHEMA; SELECT seq,ts,actor,action,COALESCE(target,''),details,success FROM audit ORDER BY seq" | python3 -c '
import sys, hashlib
prev = "0" * 64; out = ["SET search_path=%s; ALTER TABLE audit DISABLE TRIGGER USER;" % sys.argv[1]]
q = lambda x: "\x27" + x.replace("\x27", "\x27\x27") + "\x27"
for line in sys.stdin.read().split("\n"):
    if not line: continue
    seq, ts, actor, action, target, details, success = line.split("\x1f")
    if seq == "3": actor = "attacker"
    h = hashlib.sha256()
    for p in [prev, seq, ts, actor, action, target, details, "1" if success in ("1", "t") else "0"]:
        b = p.encode(); h.update(len(b).to_bytes(8, "little")); h.update(b)
    hh = h.hexdigest(); out.append("UPDATE audit SET actor=%s, prev_hash=%s, hash=%s WHERE seq=%s;" % (q(actor), q(prev), q(hh), seq)); prev = hh
out.append("ALTER TABLE audit ENABLE TRIGGER USER;"); print("\n".join(out))' "$SCHEMA" | $PSQL ;;
  esac
  if $BIN audit verify; then bad "audit attack '$1' NOT detected"; else echo "   ($1 detected as expected)"; fi
  echo "SET search_path=$SCHEMA; ALTER TABLE audit DISABLE TRIGGER USER; DELETE FROM audit; INSERT INTO audit SELECT * FROM audit_bak; DROP TABLE audit_bak; ALTER TABLE audit ENABLE TRIGGER USER;" | $PSQL
}
fi
attack edit edit; attack truncate truncate; attack rechain rechain
$BIN audit verify || bad "restored audit chain did not verify"

hr "result"
if [ $FAILS -ne 0 ]; then echo "E2E FAILED: $FAILS problem(s)"; exit 1; fi
echo "E2E OK — workspace kept at $CMSQ_HOME"
