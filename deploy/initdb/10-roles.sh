#!/bin/bash
# Runs once on an empty data dir: least-privilege runtime role.
set -euo pipefail
APP_PW=$(cat "$CMSQ_APP_PASSWORD_FILE")
psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -v app_pw="$APP_PW" <<'SQL'
CREATE ROLE cmsq_app LOGIN PASSWORD :'app_pw' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
GRANT CONNECT ON DATABASE cmsq TO cmsq_app;
GRANT USAGE ON SCHEMA public TO cmsq_app;
SQL
