# Community strategy scraping loop

`cmsq scrape [--every 6h] [--cycles N] [--symbol FUT_RB] [--backtests 3] [--backend docker]`

Each cycle is a `research_run` of kind `scrape`:

1. For each source in `<home>/sources.json` (defaults: `vnpy/vnpy_ctastrategy` strategies dir, two GitHub
   code-search queries), fetch up to `quota(base, score)` candidate files.
2. Keep only vn.py CTA strategies (`class X(CtaTemplate)` + `def on_bar`, ≤ 64 KB), wrap as
   `cmspark.scrape.v1` drops (site = source id, licence label recorded), ingest with content-hash dedupe.
3. Heuristic extraction; sandboxed backtests of new items first, then the backlog of earlier scraped items
   that were extracted but never attempted, capped at `--backtests` per cycle.
4. Per-source audit (`scrape_source`) and cycle audit (`scrape_cycle`); one failing source never stops the cycle.

## Source quality score

Stage-wise smoothed rates, so un-backtested items are not counted as failures:

    score = 0.2·(extracted+1)/(items+2) + 0.3·(backtested+1)/(attempted+2) + 0.5·(gate_passed+0.5)/(backtested+1)

`quota = base·(0.25 + 1.5·score)`, clamped to `[1, 2·base]`. `cmsq source-scores` prints the table.

## Safety

- Fetch allowlist: `https://api.github.com`, `https://raw.githubusercontent.com` only, including after redirects; body size cap.
- Scraped code is never executed outside the sandbox; it only goes through the heuristic extractor and the hardened runner.
- Optional GitHub token from `<secrets>/github_token` (never logged). Without it, the unauthenticated rate limit applies (403 is recorded per source).
- Licence (SPDX) is stored as a label; scraped code is for local research only, so check the licence before you redistribute any of it.
