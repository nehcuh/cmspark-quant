//! CMspark scrape-result drop adapter. Spec: docs/DROP_FORMAT.md (`cmspark.scrape.v1`).
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SPEC: &str = "cmspark.scrape.v1";
pub const MAX_DROP_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropSource {
    pub site: String,
    pub url: Option<String>,
    pub author: Option<String>,
    #[serde(default)]
    pub community: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropScraper {
    pub agent: String,
    pub version: Option<String>,
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropContent {
    pub title: String,
    #[serde(default = "md")]
    pub format: String,
    pub body: String,
}
fn md() -> String {
    "markdown".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropAttachment {
    pub name: String,
    pub language: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrapeDrop {
    pub spec: String,
    #[serde(default)]
    pub sample: bool,
    pub source: DropSource,
    pub scraped_at: String,
    pub scraper: DropScraper,
    pub content: DropContent,
    #[serde(default)]
    pub attachments: Vec<DropAttachment>,
    #[serde(default)]
    pub labels: Vec<String>,
}

impl ScrapeDrop {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_DROP_BYTES {
            bail!(
                "drop too large: {} bytes (max {MAX_DROP_BYTES})",
                bytes.len()
            );
        }
        let d: ScrapeDrop = serde_json::from_slice(bytes).context("invalid drop JSON")?;
        if d.spec != SPEC {
            bail!("unsupported spec '{}' (expected {SPEC})", d.spec);
        }
        if d.content.title.trim().is_empty() || d.content.body.trim().is_empty() {
            bail!("content.title and content.body are required");
        }
        chrono::DateTime::parse_from_rfc3339(&d.scraped_at)
            .context("scraped_at must be RFC3339")?;
        if let Some(u) = &d.source.url {
            if !(u.starts_with("https://") || u.starts_with("http://")) {
                bail!("source.url must be http(s)");
            }
        }
        Ok(d)
    }

    /// Content identity used for de-duplication (source url + body + attachments).
    pub fn content_sha256(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.source.url.clone().unwrap_or_default());
        h.update([0]);
        h.update(&self.content.body);
        for a in &self.attachments {
            h.update([0]);
            h.update(&a.content);
        }
        hex::encode(h.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/posts/ma_crossover.json"
        ))
        .unwrap()
    }
    #[test]
    fn parses_fixture() {
        let d = ScrapeDrop::parse(sample().as_bytes()).unwrap();
        assert!(d.sample);
        assert_eq!(d.source.site, "quant-forum");
    }
    #[test]
    fn rejects_bad_spec_url_and_size() {
        let s = sample().replace("cmspark.scrape.v1", "v0");
        assert!(ScrapeDrop::parse(s.as_bytes()).is_err());
        let s = sample().replace("https://example.invalid", "file:///etc");
        assert!(ScrapeDrop::parse(s.as_bytes()).is_err());
        let big = vec![b' '; (MAX_DROP_BYTES + 1) as usize];
        assert!(ScrapeDrop::parse(&big).is_err());
    }
}
