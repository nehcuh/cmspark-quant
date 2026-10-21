//! Server list sources.

use crate::error::{Error, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Server information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub host: String,
    pub port: u16,
    pub region: Option<String>,
    pub source: String,
}

impl ServerInfo {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Trait for fetching server lists from various sources.
#[async_trait]
pub trait ServerSource: Send + Sync {
    async fn fetch_servers(&self) -> Result<Vec<ServerInfo>>;
    fn source_name(&self) -> &str;
}

/// Static list source for predefined servers.
pub struct StaticListSource {
    servers: Vec<ServerInfo>,
    name: String,
}

impl StaticListSource {
    pub fn new(servers: Vec<ServerInfo>) -> Self {
        Self {
            servers,
            name: "static".to_string(),
        }
    }

    pub fn with_name(name: impl Into<String>, servers: Vec<ServerInfo>) -> Self {
        Self {
            servers,
            name: name.into(),
        }
    }

    /// Create from a simple list of (host, port) tuples.
    pub fn from_list(list: &[(&str, u16)]) -> Self {
        let servers = list
            .iter()
            .map(|(host, port)| ServerInfo {
                host: host.to_string(),
                port: *port,
                region: None,
                source: "static".to_string(),
            })
            .collect();
        Self::new(servers)
    }
}

#[async_trait]
impl ServerSource for StaticListSource {
    async fn fetch_servers(&self) -> Result<Vec<ServerInfo>> {
        Ok(self.servers.clone())
    }

    fn source_name(&self) -> &str {
        &self.name
    }
}

/// HTTP API source for fetching servers from remote URLs.
pub struct HttpSource {
    client: reqwest::Client,
    url: String,
    name: String,
}

impl HttpSource {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.into(),
            name: "http".to_string(),
        }
    }

    pub fn with_name(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.into(),
            name: name.into(),
        }
    }
}

#[async_trait]
impl ServerSource for HttpSource {
    async fn fetch_servers(&self) -> Result<Vec<ServerInfo>> {
        let response = self.client.get(&self.url).send().await?;

        if !response.status().is_success() {
            return Err(Error::Connection(format!(
                "HTTP error: {}",
                response.status()
            )));
        }

        let text = response.text().await?;

        // Try parsing as JSON array first
        if let Ok(servers) = serde_json::from_str::<Vec<ServerInfo>>(&text) {
            return Ok(servers);
        }

        // Fallback: parse as plain text (one server per line)
        let servers = text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| {
                let parts: Vec<&str> = line.trim().split(':').collect();
                if parts.len() >= 2 {
                    Some(ServerInfo {
                        host: parts[0].to_string(),
                        port: parts[1].parse().unwrap_or(7709),
                        region: parts.get(2).map(|s| s.to_string()),
                        source: self.name.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(servers)
    }

    fn source_name(&self) -> &str {
        &self.name
    }
}

/// Default TDX servers (from pytdx project - multiple brokers and regions).
/// Source: pytdx-1.72r2 whl package
/// Port 80 servers are listed first as they are more reliable
pub const DEFAULT_TDX_SERVERS: &[(&str, u16)] = &[
    // Port 80 servers (more reliable)
    ("180.153.18.172", 80),
    ("202.108.253.139", 80),
    // 长城国瑞
    ("218.85.139.19", 7709),
    ("218.85.139.20", 7709),
    ("58.23.131.163", 7709),
    // 上证云
    ("218.6.170.47", 7709),
    ("123.125.108.14", 7709),
    // 上海电信
    ("180.153.18.170", 7709),
    ("180.153.18.171", 7709),
    ("180.153.39.51", 7709),
    ("114.80.63.12", 7709),
    ("114.80.63.35", 7709),
    // 北京联通
    ("202.108.253.130", 7709),
    ("202.108.253.131", 7709),
    // 杭州电信
    ("60.191.117.167", 7709),
    ("115.238.56.198", 7709),
    ("218.75.126.9", 7709),
    ("115.238.90.165", 7709),
    ("124.160.88.183", 7709),
    ("60.12.136.250", 7709),
    ("218.108.98.244", 7709),
    ("218.108.47.69", 7709),
    ("223.94.89.115", 7709),
    // 青岛
    ("218.57.11.101", 7709),
    ("58.58.33.123", 7709),
    // 深圳电信
    ("14.17.75.71", 7709),
    // 招商证券
    ("119.147.212.81", 7709),
    // 华泰证券
    ("221.231.141.60", 7709),
    ("101.227.73.20", 7709),
    ("101.227.77.254", 7709),
    ("14.215.128.18", 7709),
    ("59.173.18.140", 7709),
    ("60.28.23.80", 7709),
    ("218.60.29.136", 7709),
    ("122.192.35.44", 7709),
    // 安信证券
    ("112.95.140.74", 7709),
    ("112.95.140.92", 7709),
    ("112.95.140.93", 7709),
    ("114.80.149.19", 7709),
    ("114.80.149.21", 7709),
    ("114.80.149.22", 7709),
    ("114.80.149.91", 7709),
    ("114.80.149.92", 7709),
    ("121.14.104.60", 7709),
    ("121.14.104.66", 7709),
    ("123.126.133.13", 7709),
    ("123.126.133.14", 7709),
    ("123.126.133.21", 7709),
    ("211.139.150.61", 7709),
    ("59.36.5.11", 7709),
    // 广发证券
    ("119.29.19.242", 7709),
    ("123.138.29.107", 7709),
    ("123.138.29.108", 7709),
    ("124.232.142.29", 7709),
    ("183.57.72.11", 7709),
    ("183.57.72.12", 7709),
    ("183.57.72.13", 7709),
    ("183.57.72.15", 7709),
    ("183.57.72.21", 7709),
    ("183.57.72.22", 7709),
    ("183.57.72.23", 7709),
    ("183.57.72.24", 7709),
    ("183.60.224.177", 7709),
    ("183.60.224.178", 7709),
    // 国泰君安
    ("113.105.92.99", 7709),
    ("113.105.92.100", 7709),
    ("113.105.92.101", 7709),
    ("113.105.92.102", 7709),
    ("113.105.92.103", 7709),
    ("113.105.92.104", 7709),
    ("117.34.114.13", 7709),
    ("117.34.114.14", 7709),
    ("117.34.114.15", 7709),
    ("117.34.114.16", 7709),
    ("117.34.114.17", 7709),
    ("117.34.114.18", 7709),
    ("117.34.114.20", 7709),
    ("117.34.114.27", 7709),
    ("117.34.114.30", 7709),
    ("117.34.114.31", 7709),
    // 国信证券
    ("182.131.3.252", 7709),
    ("183.60.224.11", 7709),
    ("58.210.106.91", 7709),
    ("58.63.254.216", 7709),
    ("58.63.254.219", 7709),
    ("58.63.254.247", 7709),
    // 海通证券
    ("123.125.108.90", 7709),
    ("175.6.5.153", 7709),
    ("182.118.47.151", 7709),
    ("182.131.3.245", 7709),
    ("202.100.166.27", 7709),
    ("222.161.249.156", 7709),
    ("42.123.69.62", 7709),
    ("58.63.254.191", 7709),
    ("58.63.254.217", 7709),
    // 华林证券
    ("120.55.172.97", 7709),
    ("139.217.20.27", 7709),
    ("202.100.166.21", 7709),
    ("202.96.138.90", 7709),
    ("218.106.92.182", 7709),
    ("218.106.92.183", 7709),
    ("220.178.55.71", 7709),
    ("220.178.55.86", 7709),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_info_addr() {
        let info = ServerInfo {
            host: "127.0.0.1".to_string(),
            port: 7709,
            region: None,
            source: "test".to_string(),
        };
        assert_eq!(info.addr(), "127.0.0.1:7709");
    }

    #[test]
    fn test_static_list_source() {
        let source = StaticListSource::from_list(&[("127.0.0.1", 7709), ("127.0.0.2", 7710)]);
        assert_eq!(source.servers.len(), 2);
    }

    #[test]
    fn test_server_info_clone() {
        let info = ServerInfo {
            host: "127.0.0.1".to_string(),
            port: 7709,
            region: Some("CN".to_string()),
            source: "test".to_string(),
        };
        let cloned = info.clone();
        assert_eq!(cloned.host, "127.0.0.1");
        assert_eq!(cloned.port, 7709);
    }

    #[test]
    fn test_static_list_source_empty() {
        let source = StaticListSource::from_list(&[]);
        assert!(source.servers.is_empty());
    }

    #[test]
    fn test_default_servers_not_empty() {
        assert!(!DEFAULT_TDX_SERVERS.is_empty());
    }
}
