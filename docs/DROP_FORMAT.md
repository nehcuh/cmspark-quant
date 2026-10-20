# CMspark scrape-result drop format — `cmspark.scrape.v1`

CMspark (browser agent) writes one JSON file per scraped post into `<CMSQ_HOME>/drop/`.
`cmsq ingest` (or the dashboard **Ingest** button) hands each file over to Core: the original
bytes are archived immutably at `raw/<uuid>/post.json`, the file is moved to `drop/processed/`
(or `drop/rejected/` + `.reason.txt`), and an audit entry is written. Once dropped, CMspark never
touches the sandbox (architecture boundary from the Trade-off Judge decision).

```json
{
  "spec": "cmspark.scrape.v1",            // required, exact
  "sample": false,                        // true = fixture / demo content
  "source": {
    "site": "quant-forum",                // required, short source id
    "url": "https://…/thread/123",        // optional, must be http(s); used for 1-click provenance
    "author": "someone",                  // optional
    "community": "…"                      // optional free text
  },
  "scraped_at": "2026-10-19T09:12:00+08:00",   // required, RFC3339 with offset
  "scraper": { "agent": "cmspark", "version": "x.y", "task_id": "…" },
  "content": {
    "title": "双均线金叉死叉",              // required, non-empty
    "format": "markdown",
    "body": "…prose… ```python\n…code…\n```"   // required; python code fences are extracted
  },
  "attachments": [ { "name": "strategy.py", "language": "python", "content": "…" } ],
  "labels": ["strategy-idea"]
}
```

Rules enforced by `ingest.rs`:
* max 1 MiB per file; invalid JSON / wrong spec / missing fields / non-http URLs → rejected (audited).
* De-duplication by sha256 over (url, body, attachments) → `DUPLICATE`, original id returned.
* Nothing from the drop is used as a filesystem path; Core assigns a UUID.
* Content is untrusted data. The LLM extractor wraps it in `<post>` and is told to ignore instructions in it; the code that
  is executed is always copied verbatim from the post (never LLM-written) and only ever runs inside the sandbox.

## Runner contract for strategy code
The extracted code must define `generate_signals(df, **params) -> pd.Series` returning a target position in `[0, 1]`
per bar (long-only, A-share). `df` has `open/high/low/close/volume`, indexed by date. Default values in the
signature become the extracted parameters.

Fixtures in `fixtures/posts/*.json` are **SAMPLE** posts (labelled `"sample": true`, URLs on `example.invalid`).
