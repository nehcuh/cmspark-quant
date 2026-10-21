#!/usr/bin/env bash
# Start the OPTIONAL Redis hot layer (deploy/docker-compose.yml, profile "hot") and
# write <secrets>/redis.env (mode 0600) with CMSQ_REDIS_URL. cmsq works without it.
set -euo pipefail
# compose interpolates every service; signer settings are irrelevant here but must be set
export CMSQ_HOST_UID=${CMSQ_HOST_UID:-$(id -u)} CMSQ_HOST_GID=${CMSQ_HOST_GID:-$(id -g)} CMSQ_RUN_DIR=${CMSQ_RUN_DIR:-/tmp/cmsq-signer-unused}
cd "$(dirname "$0")/.."
export CMSQ_SECRETS_DIR=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
export CMSQ_REDIS_PORT=${CMSQ_REDIS_PORT:-56379}
mkdir -p "$CMSQ_SECRETS_DIR" && chmod 700 "$CMSQ_SECRETS_DIR"
umask 077
for f in pg_owner_pw pg_app_pw redis_pw; do
  [ -s "$CMSQ_SECRETS_DIR/$f" ] || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$CMSQ_SECRETS_DIR/$f"
done
chmod 644 "$CMSQ_SECRETS_DIR/redis_pw"
export CMSQ_PG_IMAGE_DIGEST=${CMSQ_PG_IMAGE_DIGEST:-6f139d56042989bd35f50ba5986e492cd32e35cc6778c50be2659add298dc09f}
docker compose -f deploy/docker-compose.yml --profile hot up -d --wait redis
cat > "$CMSQ_SECRETS_DIR/redis.env" <<ENV
CMSQ_REDIS_URL=redis://:$(cat "$CMSQ_SECRETS_DIR/redis_pw")@127.0.0.1:$CMSQ_REDIS_PORT/0
ENV
chmod 600 "$CMSQ_SECRETS_DIR/redis.env"
echo "Redis hot layer up on 127.0.0.1:$CMSQ_REDIS_PORT; settings in $CMSQ_SECRETS_DIR/redis.env"
