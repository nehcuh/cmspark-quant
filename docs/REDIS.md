# Optional Redis hot layer

`scripts/redis_up.sh` starts the `redis` compose service (profile `hot`, pinned digest, password from `<secrets>/redis_pw`, bound to 127.0.0.1:56379). It also writes `<secrets>/redis.env`. cmsq picks up the connection from `CMSQ_REDIS_URL` or from `redis.env`. Set `CMSQ_REDIS=off` to turn it off.

| Use | Key / channel | Fallback without Redis |
|---|---|---|
| Latest quote per instrument (from `md run` / `paper run`) | `cmsq:q:<inst>` (TTL 1h) | the `ticks` table in PostgreSQL |
| Live push to the dashboard | pub/sub `cmsq:quotes`, `cmsq:paper` | the dashboard's periodic reload |
| Shared rate limit for data sources (Tushare per API) | `cmsq:rl:tushare:<api>`, atomic Lua token bucket | per-process token bucket |
| Job queue / locks | not used: PostgreSQL advisory locks and the run lease already serialise; a second lock system would only add split-brain risk | — |

Guarantees:
- **PostgreSQL stays the source of truth.** Audit entries, evidence, paper orders and fills, and snapshots are written only to PG. Redis carries only copies or ephemeral state, so wiping it loses nothing of record.
- **Redis is never required.** If it is unreachable, a call degrades to a no-op within about 300 ms and backs off for 10 s, so callers never stall (`unreachable_redis_degrades_fast`).
- **The password never appears in logs.** It is redacted in `describe()` and in errors.
- **The server is hardened:** no persistence, `maxmemory` 256 MB with LRU eviction, and `FLUSHALL`/`CONFIG` disabled.

CI runs a live test against a Redis service container: pub/sub delivery, the quote cache, and one token bucket shared by two clients.
