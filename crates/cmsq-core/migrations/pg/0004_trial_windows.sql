-- Actual scored intervals are part of research-selection identity (#7).
-- Never rewrite append-only historical trials or infer dates from their length.
-- Empty legacy intervals are counted separately and conservatively by trial id.
ALTER TABLE trials ADD COLUMN IF NOT EXISTS score_start TEXT NOT NULL DEFAULT '';
ALTER TABLE trials ADD COLUMN IF NOT EXISTS score_end TEXT NOT NULL DEFAULT '';
