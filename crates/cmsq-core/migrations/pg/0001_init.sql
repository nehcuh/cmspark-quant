-- cmsq PostgreSQL schema v1. Run by the OWNER role (scripts/db_up.sh / `cmsq db migrate`);
-- the app role (cmsq_app) only gets INSERT/SELECT on the evidence tables.
CREATE TABLE IF NOT EXISTS strategies(
  id TEXT PRIMARY KEY, title TEXT NOT NULL, source_site TEXT NOT NULL, source_url TEXT,
  author TEXT, scraped_at TEXT, ingested_at TEXT NOT NULL, raw_path TEXT NOT NULL,
  content_sha256 TEXT NOT NULL UNIQUE, sample BIGINT NOT NULL DEFAULT 0,
  status TEXT NOT NULL, attempts BIGINT NOT NULL DEFAULT 0, last_reason TEXT,
  extracted_json TEXT, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS jobs(
  id TEXT PRIMARY KEY, strategy_id TEXT NOT NULL REFERENCES strategies(id), backend TEXT NOT NULL,
  image TEXT NOT NULL, symbol TEXT NOT NULL, data_source TEXT NOT NULL, status TEXT NOT NULL,
  exit_code BIGINT, started_at TEXT NOT NULL, finished_at TEXT, duration_ms BIGINT,
  job_dir TEXT NOT NULL, error TEXT, result_json TEXT,
  owner_pid BIGINT, runner_identity TEXT, kind TEXT NOT NULL DEFAULT 'dev', owner_host TEXT);
CREATE INDEX IF NOT EXISTS jobs_strategy ON jobs(strategy_id);
CREATE TABLE IF NOT EXISTS trials(
  id BIGSERIAL PRIMARY KEY, strategy_id TEXT NOT NULL, job_id TEXT NOT NULL,
  idea_key TEXT NOT NULL, params_hash TEXT NOT NULL, symbol TEXT NOT NULL, dataset_sha TEXT NOT NULL,
  bars BIGINT NOT NULL, sharpe_daily DOUBLE PRECISION NOT NULL, positions_json TEXT NOT NULL, created_at TEXT NOT NULL,
  lookahead BIGINT NOT NULL DEFAULT 0, run_kind TEXT NOT NULL DEFAULT 'single', group_id TEXT);
CREATE TABLE IF NOT EXISTS vault_touches(
  strategy_id TEXT PRIMARY KEY, job_id TEXT NOT NULL, passed BIGINT NOT NULL, created_at TEXT NOT NULL,
  idea_key TEXT, symbol TEXT, code_sha TEXT, finished BIGINT NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS evaluations(
  id TEXT PRIMARY KEY, strategy_id TEXT NOT NULL REFERENCES strategies(id), job_id TEXT NOT NULL,
  passed BIGINT NOT NULL, reasons_json TEXT NOT NULL, checks_json TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS signals(
  id BIGSERIAL PRIMARY KEY, strategy_id TEXT NOT NULL, job_id TEXT NOT NULL,
  symbol TEXT NOT NULL, date TEXT NOT NULL, target_position DOUBLE PRECISION NOT NULL,
  observe_only BIGINT NOT NULL DEFAULT 1 CHECK(observe_only = 1), created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS audit(
  seq BIGINT PRIMARY KEY, ts TEXT NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
  target TEXT, details TEXT NOT NULL, success BIGINT NOT NULL, prev_hash TEXT NOT NULL, hash TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS audit_target ON audit(target, action);
CREATE TABLE IF NOT EXISTS research_runs(
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, strategy_id TEXT, spec_json TEXT NOT NULL,
  result_json TEXT, status TEXT NOT NULL, created_at TEXT NOT NULL, finished_at TEXT);
CREATE TABLE IF NOT EXISTS bars(
  symbol TEXT NOT NULL, date DATE NOT NULL, snapshot_sha TEXT NOT NULL,
  open DOUBLE PRECISION NOT NULL, high DOUBLE PRECISION NOT NULL, low DOUBLE PRECISION NOT NULL,
  close DOUBLE PRECISION NOT NULL, volume DOUBLE PRECISION NOT NULL, adj_factor DOUBLE PRECISION,
  PRIMARY KEY(symbol, snapshot_sha, date));
CREATE TABLE IF NOT EXISTS ticks(
  instrument TEXT NOT NULL, ts TIMESTAMPTZ NOT NULL, trading_day TEXT NOT NULL,
  last DOUBLE PRECISION NOT NULL, volume DOUBLE PRECISION NOT NULL, open_interest DOUBLE PRECISION,
  bid1 DOUBLE PRECISION, ask1 DOUBLE PRECISION, source TEXT NOT NULL);

-- append-only evidence: no UPDATE / DELETE / TRUNCATE, enforced in the database
CREATE OR REPLACE FUNCTION cmsq_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION '% is append-only', TG_TABLE_NAME; END $$;
CREATE OR REPLACE FUNCTION cmsq_vault_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN RAISE EXCEPTION 'vault_touches is append-only'; END IF;
  IF OLD.finished <> 0 THEN RAISE EXCEPTION 'vault touch already finalized'; END IF;
  IF NEW.strategy_id <> OLD.strategy_id OR NEW.job_id <> OLD.job_id OR NEW.created_at <> OLD.created_at
     OR NEW.idea_key IS DISTINCT FROM OLD.idea_key OR NEW.code_sha IS DISTINCT FROM OLD.code_sha OR NEW.symbol IS DISTINCT FROM OLD.symbol
  THEN RAISE EXCEPTION 'vault touch identity is immutable'; END IF;
  RETURN NEW;
END $$;
DO $$
DECLARE t TEXT;
BEGIN
  FOREACH t IN ARRAY ARRAY['audit','trials','signals','evaluations'] LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS %1$s_append_only ON %1$I', t);
    EXECUTE format('CREATE TRIGGER %1$s_append_only BEFORE UPDATE OR DELETE ON %1$I FOR EACH ROW EXECUTE FUNCTION cmsq_append_only()', t);
    EXECUTE format('DROP TRIGGER IF EXISTS %1$s_no_truncate ON %1$I', t);
    EXECUTE format('CREATE TRIGGER %1$s_no_truncate BEFORE TRUNCATE ON %1$I FOR EACH STATEMENT EXECUTE FUNCTION cmsq_append_only()', t);
  END LOOP;
END $$;
DROP TRIGGER IF EXISTS vault_guard ON vault_touches;
CREATE TRIGGER vault_guard BEFORE UPDATE OR DELETE ON vault_touches FOR EACH ROW EXECUTE FUNCTION cmsq_vault_guard();
DROP TRIGGER IF EXISTS vault_no_truncate ON vault_touches;
CREATE TRIGGER vault_no_truncate BEFORE TRUNCATE ON vault_touches FOR EACH STATEMENT EXECUTE FUNCTION cmsq_append_only();

-- TimescaleDB hypertables for time series when the extension is available
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'timescaledb') THEN
    BEGIN
      CREATE EXTENSION IF NOT EXISTS timescaledb SCHEMA public;
    EXCEPTION WHEN unique_violation OR duplicate_object THEN NULL; -- concurrent first install
    END;
    PERFORM public.create_hypertable(format('%I.bars', current_schema())::regclass, 'date', if_not_exists => TRUE, migrate_data => TRUE);
    PERFORM public.create_hypertable(format('%I.ticks', current_schema())::regclass, 'ts', if_not_exists => TRUE, migrate_data => TRUE);
  END IF;
END $$;

-- least privilege for the runtime role
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'cmsq_app') THEN
    EXECUTE format('GRANT USAGE ON SCHEMA %I TO cmsq_app', current_schema());
    GRANT SELECT, INSERT ON audit, trials, signals, evaluations, bars, ticks TO cmsq_app;
    GRANT SELECT, INSERT, UPDATE ON strategies, jobs, research_runs TO cmsq_app;
    GRANT SELECT, INSERT ON vault_touches TO cmsq_app;
    GRANT UPDATE (passed, finished) ON vault_touches TO cmsq_app;
    EXECUTE format('GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA %I TO cmsq_app', current_schema());
  END IF;
END $$;
