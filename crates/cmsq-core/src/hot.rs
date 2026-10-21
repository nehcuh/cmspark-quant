//! Optional Redis hot layer. The system works without it: with `CMSQ_REDIS_URL`
//! unset, or Redis unreachable, every call is a cheap no-op and callers fall back
//! to local behaviour.
//!
//! What lives here (all ephemeral, all rebuildable):
//! * latest quote per instrument (`cmsq:q:<inst>`, TTL) for viewers;
//! * pub/sub: `cmsq:quotes` (live quotes), `cmsq:paper` (simulated fills/settlements);
//! * distributed token buckets for data sources (`cmsq:rl:<bucket>`), so several
//!   processes share one per-minute budget.
//!
//! What never lives here: audit entries, evidence, orders/fills of record. PostgreSQL
//! stays the source of truth; Redis loss loses nothing that matters.
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const CH_QUOTES: &str = "cmsq:quotes";
pub const CH_PAPER: &str = "cmsq:paper";

pub struct Hot {
    inner: Option<Mutex<Conn>>,
    url_redacted: String,
}

struct Conn {
    client: redis::Client,
    conn: Option<redis::Connection>,
    /// after a failure, skip Redis until this instant (avoid stalling callers)
    down_until: Option<std::time::Instant>,
}

/// Atomic token bucket. KEYS[1]=bucket, ARGV = per_min, burst, now_ms.
/// Returns 0 when a token was taken, else the wait in ms.
const BUCKET_LUA: &str = r#"
local per_ms = tonumber(ARGV[1]) / 60000.0
local burst = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local t = redis.call('HMGET', KEYS[1], 'tokens', 'ts')
local tokens = tonumber(t[1]) or burst
local ts = tonumber(t[2]) or now
if now > ts then tokens = math.min(burst, tokens + (now - ts) * per_ms) end
local wait = 0
if tokens >= 1 then tokens = tokens - 1 else wait = math.ceil((1 - tokens) / per_ms) end
redis.call('HSET', KEYS[1], 'tokens', tostring(tokens), 'ts', tostring(math.max(now, ts)))
redis.call('PEXPIRE', KEYS[1], 600000)
return wait
"#;

fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(a), Some(b)) if b > a => format!("{}://***@{}", &url[..a], &url[b + 1..]),
        _ => url.to_string(),
    }
}

impl Hot {
    pub fn disabled() -> Self {
        Self {
            inner: None,
            url_redacted: String::new(),
        }
    }
    pub fn connect(url: &str) -> Self {
        match redis::Client::open(url) {
            Ok(client) => Self {
                inner: Some(Mutex::new(Conn {
                    client,
                    conn: None,
                    down_until: None,
                })),
                url_redacted: redact(url),
            },
            Err(e) => {
                eprintln!("cmsq: redis disabled ({}): {e}", redact(url));
                Self::disabled()
            }
        }
    }
    /// `CMSQ_REDIS_URL` (e.g. redis://:pw@127.0.0.1:56379/0), else `<secrets>/redis.env`
    /// written by scripts/redis_up.sh, else disabled. `CMSQ_REDIS=off` forces it off.
    pub fn from_env() -> Self {
        if std::env::var("CMSQ_REDIS").is_ok_and(|v| v == "off") {
            return Self::disabled();
        }
        if let Ok(u) = std::env::var("CMSQ_REDIS_URL") {
            return if u.trim().is_empty() {
                Self::disabled()
            } else {
                Self::connect(u.trim())
            };
        }
        let dir = std::env::var("CMSQ_SECRETS_DIR")
            .map(std::path::PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| std::path::Path::new(&h).join(".config/cmsq"))
            });
        let url = dir
            .and_then(|d| std::fs::read_to_string(d.join("redis.env")).ok())
            .and_then(|t| {
                t.lines().find_map(|l| {
                    l.strip_prefix("CMSQ_REDIS_URL=")
                        .map(|x| x.trim().to_string())
                })
            });
        match url {
            Some(u) if !u.is_empty() => Self::connect(&u),
            _ => Self::disabled(),
        }
    }
    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }
    pub fn describe(&self) -> String {
        if self.enabled() {
            format!("redis {}", self.url_redacted)
        } else {
            "disabled".into()
        }
    }

    fn with<T>(
        &self,
        f: impl FnOnce(&mut redis::Connection) -> redis::RedisResult<T>,
    ) -> Option<T> {
        let m = self.inner.as_ref()?;
        let mut c = m.lock().unwrap_or_else(|e| e.into_inner());
        if c.down_until.is_some_and(|t| std::time::Instant::now() < t) {
            return None;
        }
        if c.conn.is_none() {
            match c
                .client
                .get_connection_with_timeout(Duration::from_millis(300))
            {
                Ok(k) => {
                    let _ = k.set_read_timeout(Some(Duration::from_millis(500)));
                    let _ = k.set_write_timeout(Some(Duration::from_millis(500)));
                    c.conn = Some(k);
                }
                Err(_) => {
                    c.down_until = Some(std::time::Instant::now() + Duration::from_secs(10));
                    return None;
                }
            }
        }
        match f(c.conn.as_mut().unwrap()) {
            Ok(v) => {
                c.down_until = None;
                Some(v)
            }
            Err(_) => {
                c.conn = None;
                c.down_until = Some(std::time::Instant::now() + Duration::from_secs(10));
                None
            }
        }
    }

    pub fn ping(&self) -> bool {
        self.with(|c| redis::cmd("PING").query::<String>(c))
            .is_some_and(|s| s == "PONG")
    }

    /// Cache the latest quote and publish it. Best effort.
    pub fn put_quote(&self, instrument: &str, q: &Value) {
        let s = q.to_string();
        self.with(|c| {
            redis::pipe()
                .cmd("SET")
                .arg(format!("cmsq:q:{instrument}"))
                .arg(&s)
                .arg("EX")
                .arg(3600)
                .ignore()
                .cmd("PUBLISH")
                .arg(CH_QUOTES)
                .arg(&s)
                .ignore()
                .query::<()>(c)
        });
    }
    pub fn get_quote(&self, instrument: &str) -> Option<Value> {
        let s: Option<String> = self.with(|c| {
            redis::cmd("GET")
                .arg(format!("cmsq:q:{instrument}"))
                .query(c)
        })?;
        s.and_then(|s| serde_json::from_str(&s).ok())
    }
    pub fn publish(&self, channel: &str, v: &Value) {
        let s = v.to_string();
        self.with(|c| redis::cmd("PUBLISH").arg(channel).arg(&s).query::<i64>(c));
    }

    /// Distributed token bucket shared by every process using this Redis.
    /// `None` = no distributed limiter (Redis off/unreachable): use the local bucket only.
    pub fn take_token(&self, bucket: &str, per_min: f64) -> Option<Duration> {
        let burst = (per_min / 10.0).max(1.0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as i64;
        let ms: i64 = self.with(|c| {
            redis::Script::new(BUCKET_LUA)
                .key(format!("cmsq:rl:{bucket}"))
                .arg(per_min)
                .arg(burst)
                .arg(now)
                .invoke(c)
        })?;
        Some(Duration::from_millis(ms.max(0) as u64))
    }

    /// Subscribe (blocking) to channels; calls `f` per message until it returns false
    /// or the connection drops. Used by the dashboard's live panel in a thread.
    pub fn subscribe(
        &self,
        channels: &[&str],
        mut f: impl FnMut(&str, Value) -> bool,
    ) -> anyhow::Result<()> {
        let m = self
            .inner
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("redis disabled"))?;
        let client = m.lock().unwrap_or_else(|e| e.into_inner()).client.clone();
        let mut conn = client.get_connection_with_timeout(Duration::from_secs(2))?;
        let mut ps = conn.as_pubsub();
        for ch in channels {
            ps.subscribe(*ch)?;
        }
        loop {
            let msg = ps.get_message()?;
            let ch = msg.get_channel_name().to_string();
            let payload: String = msg.get_payload()?;
            if !f(
                &ch,
                serde_json::from_str(&payload).unwrap_or(Value::String(payload)),
            ) {
                return Ok(());
            }
        }
    }
}

/// Process-wide instance from the environment.
pub fn global() -> &'static Hot {
    static H: OnceLock<Hot> = OnceLock::new();
    H.get_or_init(Hot::from_env)
}

#[cfg(test)]
mod tests {
    /// Real Redis (scripts/redis_up.sh or CI service): CMSQ_TEST_REDIS_URL.
    #[test]
    #[ignore]
    fn redis_live_quote_pubsub_and_shared_bucket() {
        let url = std::env::var("CMSQ_TEST_REDIS_URL").expect("CMSQ_TEST_REDIS_URL");
        let h = Hot::connect(&url);
        assert!(h.ping());
        let inst = format!("t{}", std::process::id());
        // subscriber in a thread receives a published quote
        let (tx, rx) = std::sync::mpsc::channel();
        let u2 = url.clone();
        let i2 = inst.clone();
        std::thread::spawn(move || {
            let s = Hot::connect(&u2);
            let _ = s.subscribe(&[CH_QUOTES], |_, v| {
                if v["instrument"] == i2.as_str() {
                    let _ = tx.send(v);
                    return false;
                }
                true
            });
        });
        std::thread::sleep(Duration::from_millis(300));
        h.put_quote(
            &inst,
            &serde_json::json!({"instrument": inst, "last": 3500.0}),
        );
        let got = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("pub/sub delivery");
        assert_eq!(got["last"], 3500.0);
        assert_eq!(h.get_quote(&inst).unwrap()["last"], 3500.0);
        // two processes' worth of clients share ONE bucket: 60/min ⇒ burst 6
        let b = format!("test:{inst}");
        let h2 = Hot::connect(&url);
        let mut ok = 0;
        for i in 0..10 {
            let c = if i % 2 == 0 { &h } else { &h2 };
            if c.take_token(&b, 60.0).unwrap().is_zero() {
                ok += 1;
            }
        }
        assert_eq!(ok, 6, "burst shared across clients");
        let w = h.take_token(&b, 60.0).unwrap();
        assert!(
            w > Duration::from_millis(500) && w <= Duration::from_millis(1100),
            "{w:?}"
        );
    }

    use super::*;
    #[test]
    fn disabled_is_a_noop() {
        let h = Hot::disabled();
        assert!(!h.enabled() && !h.ping());
        h.put_quote("rb2601", &serde_json::json!({"last": 1}));
        assert!(h.get_quote("rb2601").is_none());
        assert!(h.take_token("x", 60.0).is_none());
    }
    #[test]
    fn unreachable_redis_degrades_fast() {
        let h = Hot::connect("redis://127.0.0.1:1/0");
        assert!(h.enabled());
        let t = std::time::Instant::now();
        assert!(h.take_token("x", 60.0).is_none());
        h.put_quote("a", &serde_json::json!({}));
        assert!(h.get_quote("a").is_none());
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "must not stall callers"
        );
    }
    #[test]
    fn password_is_redacted() {
        assert_eq!(redact("redis://:secret@h:1/0"), "redis://***@h:1/0");
        assert!(!Hot::connect("redis://:secret@127.0.0.1:1/0")
            .describe()
            .contains("secret"));
    }
}
