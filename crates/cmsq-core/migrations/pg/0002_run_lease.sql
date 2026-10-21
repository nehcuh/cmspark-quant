-- issue #8: run lease taken atomically with the Backtesting status change
ALTER TABLE strategies ADD COLUMN IF NOT EXISTS run_owner TEXT;
