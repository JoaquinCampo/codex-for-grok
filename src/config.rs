use std::{env, net::IpAddr, path::PathBuf, time::Duration};

use reqwest::Url;

pub const DEFAULT_PORT: u16 = 18_474;
pub const DEFAULT_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_MAX_STREAMS: usize = 16;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 180;
pub const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;
pub const CODEX_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Clone, Debug)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    pub auth_path: PathBuf,
    pub upstream_url: Url,
    pub max_body_bytes: usize,
    pub max_streams: usize,
    pub upstream_idle_timeout: Duration,
    pub drain_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let host = env_value("CODEX_FOR_GROK_HOST", "127.0.0.1")
            .parse::<IpAddr>()
            .map_err(|error| format!("CODEX_FOR_GROK_HOST must be an IP address: {error}"))?;
        if !host.is_loopback() {
            return Err("CODEX_FOR_GROK_HOST must be a loopback address".to_owned());
        }

        let port = parse_env("CODEX_FOR_GROK_PORT", DEFAULT_PORT)?;
        let max_body_bytes = parse_env("CODEX_FOR_GROK_MAX_BODY_BYTES", DEFAULT_MAX_BODY_BYTES)?;
        let max_streams = parse_env("CODEX_FOR_GROK_MAX_STREAMS", DEFAULT_MAX_STREAMS)?;
        if max_body_bytes == 0 || max_streams == 0 {
            return Err("bridge request and stream limits must be greater than zero".to_owned());
        }

        let idle_timeout_secs = parse_env(
            "CODEX_FOR_GROK_UPSTREAM_IDLE_TIMEOUT_SECS",
            DEFAULT_IDLE_TIMEOUT_SECS,
        )?;
        let drain_timeout_secs = parse_env(
            "CODEX_FOR_GROK_DRAIN_TIMEOUT_SECS",
            DEFAULT_DRAIN_TIMEOUT_SECS,
        )?;

        let upstream_url = env_value("CODEX_FOR_GROK_UPSTREAM_URL", CODEX_URL)
            .parse::<Url>()
            .map_err(|error| format!("invalid CODEX_FOR_GROK_UPSTREAM_URL: {error}"))?;
        if upstream_url.scheme() != "https" {
            return Err("CODEX_FOR_GROK_UPSTREAM_URL must use HTTPS".to_owned());
        }

        let auth_path = env::var_os("CODEX_AUTH_PATH")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|home| home.join(".codex/auth.json")))
            .ok_or_else(|| "could not determine the Codex auth path".to_owned())?;

        Ok(Self {
            host,
            port,
            auth_path,
            upstream_url,
            max_body_bytes,
            max_streams,
            upstream_idle_timeout: Duration::from_secs(idle_timeout_secs),
            drain_timeout: Duration::from_secs(drain_timeout_secs),
        })
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            host: "127.0.0.1".parse().expect("test loopback IP is valid"),
            port: DEFAULT_PORT,
            auth_path: PathBuf::from("/tmp/auth.json"),
            upstream_url: CODEX_URL.parse().expect("default upstream URL is valid"),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_streams: DEFAULT_MAX_STREAMS,
            upstream_idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
            drain_timeout: Duration::from_secs(DEFAULT_DRAIN_TIMEOUT_SECS),
        }
    }
}

fn env_value(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn parse_env<T>(name: &str, default: T) -> Result<T, String>
where
    T: std::str::FromStr + Copy + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| format!("{name} must be a valid value: {error}")),
        Err(_) => Ok(default),
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}
