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
for f in pg_owner_pw pg_app_pw; do
  [ -s "$CMSQ_SECRETS_DIR/$f" ] || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$CMSQ_SECRETS_DIR/$f"
done
# the postgres container user must be able to read the compose secrets
chmod 644 "$CMSQ_SECRETS_DIR/pg_owner_pw" "$CMSQ_SECRETS_DIR/pg_app_pw" 2>/dev/null || true
docker compose -f deploy/docker-compose.yml up -d --wait db
OW=$(cat "$CMSQ_SECRETS_DIR/pg_owner_pw"); AP=$(cat "$CMSQ_SECRETS_DIR/pg_app_pw")
cat > "$CMSQ_SECRETS_DIR/db.env" <<ENV
CMSQ_DATABASE_URL=postgres://cmsq_app:$AP@127.0.0.1:$CMSQ_PG_PORT/cmsq
CMSQ_DATABASE_MIGRATE_URL=postgres://cmsq_owner:$OW@127.0.0.1:$CMSQ_PG_PORT/cmsq
ENV
chmod 600 "$CMSQ_SECRETS_DIR/db.env"
echo "PostgreSQL+TimescaleDB up on 127.0.0.1:$CMSQ_PG_PORT; connection settings in $CMSQ_SECRETS_DIR/db.env"
