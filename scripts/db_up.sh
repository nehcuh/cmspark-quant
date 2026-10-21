#!/usr/bin/env bash
# Start the independently deployed PostgreSQL/TimescaleDB (deploy/docker-compose.yml),
# generate role passwords on first run and write <secrets>/db.env (mode 0600):
#   CMSQ_DATABASE_URL          runtime role cmsq_app (least privilege)
#   CMSQ_DATABASE_MIGRATE_URL  owner role (migrations only)
# Secrets dir: $CMSQ_SECRETS_DIR or ~/.config/cmsq — never inside a workspace or the repo.
set -euo pipefail
# compose interpolates every service; signer settings are irrelevant here but must be set
export CMSQ_HOST_UID=${CMSQ_HOST_UID:-$(id -u)} CMSQ_HOST_GID=${CMSQ_HOST_GID:-$(id -g)} CMSQ_RUN_DIR=${CMSQ_RUN_DIR:-/tmp/cmsq-signer-unused}
cd "$(dirname "$0")/.."
export CMSQ_SECRETS_DIR=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
export CMSQ_PG_PORT=${CMSQ_PG_PORT:-55432}
export CMSQ_PG_IMAGE_DIGEST=${CMSQ_PG_IMAGE_DIGEST:-6f139d56042989bd35f50ba5986e492cd32e35cc6778c50be2659add298dc09f}
mkdir -p "$CMSQ_SECRETS_DIR" && chmod 700 "$CMSQ_SECRETS_DIR"
umask 077
# One PostgreSQL per host (compose project "cmsq") serves every secrets dir. If it is
# already running with another secrets dir's password files, ADOPT those (compose does
# not recreate the container for a changed secret path), instead of minting new ones
# that the server would never see.
RUNNING_SRC=$(docker inspect cmsq-db-1 --format '{{range .Mounts}}{{if eq .Destination "/run/secrets/pg_owner_pw"}}{{.Source}}{{end}}{{end}}' 2>/dev/null || true)
if [ -n "$RUNNING_SRC" ]; then
  SRC_DIR=$(dirname "$RUNNING_SRC")
  if [ "$(readlink -f "$SRC_DIR")" != "$(readlink -f "$CMSQ_SECRETS_DIR")" ]; then
    [ -r "$SRC_DIR/pg_owner_pw" ] && [ -r "$SRC_DIR/pg_app_pw" ] || { echo "db already running with secrets in $SRC_DIR (unreadable); use CMSQ_SECRETS_DIR=$SRC_DIR" >&2; exit 2; }
    for f in pg_owner_pw pg_app_pw; do
      if [ -s "$CMSQ_SECRETS_DIR/$f" ] && ! cmp -s "$SRC_DIR/$f" "$CMSQ_SECRETS_DIR/$f"; then mv "$CMSQ_SECRETS_DIR/$f" "$CMSQ_SECRETS_DIR/$f.stale.$(date +%s)"; fi
      install -m 600 "$SRC_DIR/$f" "$CMSQ_SECRETS_DIR/$f"
    done
    echo "db is running with credentials from $SRC_DIR: adopted them into $CMSQ_SECRETS_DIR"
    export CMSQ_SECRETS_DIR_COMPOSE=$SRC_DIR
  fi
fi
for f in pg_owner_pw pg_app_pw; do
  [ -s "$CMSQ_SECRETS_DIR/$f" ] || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$CMSQ_SECRETS_DIR/$f"
done
# the postgres container user must be able to read the compose secrets
[ -n "${CMSQ_SECRETS_DIR_COMPOSE:-}" ] || chmod 644 "$CMSQ_SECRETS_DIR/pg_owner_pw" "$CMSQ_SECRETS_DIR/pg_app_pw" 2>/dev/null || true
CMSQ_SECRETS_DIR=${CMSQ_SECRETS_DIR_COMPOSE:-$CMSQ_SECRETS_DIR} docker compose -f deploy/docker-compose.yml up -d --wait db
OW=$(cat "$CMSQ_SECRETS_DIR/pg_owner_pw"); AP=$(cat "$CMSQ_SECRETS_DIR/pg_app_pw")
# Self-heal: postgres applies POSTGRES_PASSWORD_FILE only when the volume is first
# initialised, so a volume created under another secrets dir keeps the old passwords
# and db.env would not match them. Reconcile both roles to THIS secrets dir (compose
# recreates the container when the secret paths change, so /run/secrets is current),
# over the container's local socket. psql reads the secrets itself (no argv), and the
# session turns statement logging off so the passwords never reach the server log.
CMSQ_SECRETS_DIR=${CMSQ_SECRETS_DIR_COMPOSE:-$CMSQ_SECRETS_DIR} docker compose -f deploy/docker-compose.yml exec -T db psql -X -v ON_ERROR_STOP=1 -q -U cmsq_owner -d cmsq >/dev/null <<'SQL'
\set ow `cat /run/secrets/pg_owner_pw`
\set ap `cat /run/secrets/pg_app_pw`
SET log_statement = 'none';
ALTER ROLE cmsq_owner PASSWORD :'ow';
ALTER ROLE cmsq_app PASSWORD :'ap';
SQL
cat > "$CMSQ_SECRETS_DIR/db.env" <<ENV
CMSQ_DATABASE_URL=postgres://cmsq_app:$AP@127.0.0.1:$CMSQ_PG_PORT/cmsq
CMSQ_DATABASE_MIGRATE_URL=postgres://cmsq_owner:$OW@127.0.0.1:$CMSQ_PG_PORT/cmsq
ENV
chmod 600 "$CMSQ_SECRETS_DIR/db.env"
echo "PostgreSQL+TimescaleDB up on 127.0.0.1:$CMSQ_PG_PORT; connection settings in $CMSQ_SECRETS_DIR/db.env"
