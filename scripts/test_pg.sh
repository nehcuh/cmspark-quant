#!/usr/bin/env bash
# PostgreSQL race / privilege tests against the deployed database (scripts/db_up.sh).
set -euo pipefail
cd "$(dirname "$0")/.."
export CMSQ_SECRETS_DIR=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
[ -s "$CMSQ_SECRETS_DIR/db.env" ] || scripts/db_up.sh
export CMSQ_TEST_DB_ENV=$CMSQ_SECRETS_DIR/db.env
cargo test -q -p cmsq-core --test store_concurrency -- --include-ignored
cargo test -q -p cmsq-core --test pipeline pg_ -- --include-ignored
