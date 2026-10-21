-- paper trading (simulated fills; cmsq-paper). Events and snapshots are append-only.
CREATE TABLE IF NOT EXISTS paper_sessions(
  id TEXT PRIMARY KEY, strategy_id TEXT NOT NULL, config_json TEXT NOT NULL, status TEXT NOT NULL,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS paper_events(
  session_id TEXT NOT NULL, seq BIGINT NOT NULL, kind TEXT NOT NULL, event_json TEXT NOT NULL,
  chain TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(session_id, seq));
CREATE TABLE IF NOT EXISTS paper_snapshots(
  session_id TEXT NOT NULL, seq BIGINT NOT NULL, trading_day TEXT, equity DOUBLE PRECISION NOT NULL,
  state_json TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(session_id, seq));
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['paper_events','paper_snapshots'] LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS %1$s_append_only ON %1$I', t);
    EXECUTE format('CREATE TRIGGER %1$s_append_only BEFORE UPDATE OR DELETE ON %1$I FOR EACH ROW EXECUTE FUNCTION cmsq_append_only()', t);
    EXECUTE format('DROP TRIGGER IF EXISTS %1$s_no_truncate ON %1$I', t);
    EXECUTE format('CREATE TRIGGER %1$s_no_truncate BEFORE TRUNCATE ON %1$I FOR EACH STATEMENT EXECUTE FUNCTION cmsq_append_only()', t);
  END LOOP;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'cmsq_app') THEN
    GRANT SELECT, INSERT ON paper_events, paper_snapshots TO cmsq_app;
    GRANT SELECT, INSERT, UPDATE ON paper_sessions TO cmsq_app;
  END IF;
END $$;
