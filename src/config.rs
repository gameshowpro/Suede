//! Bootstrap configuration: everything that must be known before the API can serve.
//!
//! Read once at startup from `$XDG_CONFIG_HOME/suede/suede.toml`; every value is
//! overridable by a `SUEDE_*` environment variable, which wins. Everything else
//! is desired state, owned by the API (see [`crate::state`]).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_BIND: &str = "0.0.0.0:7071";
pub const DEFAULT_DOCS_BASE_URL: &str = "https://suede.gameshow.pro/";

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    bind: Option<String>,
    token: Option<String>,
    state_dir: Option<PathBuf>,
    docs_base_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// Address the HTTP server binds to.
    pub bind: SocketAddr,
    /// Optional static bearer token. When set, the reference web UI is disabled.
    pub token: Option<String>,
    /// Directory holding the persisted desired state.
    pub state_dir: PathBuf,
    /// Base URL used to build `docsUrl` links in health checks.
    pub docs_base_url: String,
    /// Sway configuration file that health-check fixes may patch.
    ///
    /// Held here rather than derived at the point of use so the remediations
    /// can be exercised against a temporary directory instead of a real home.
    pub sway_config_path: PathBuf,
    /// Directory for systemd *user* units that fixes may write.
    pub systemd_user_dir: PathBuf,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("valid default bind address"),
            token: None,
            state_dir: crate::util::state_dir(),
            docs_base_url: DEFAULT_DOCS_BASE_URL.to_string(),
            sway_config_path: default_sway_config_path(),
            systemd_user_dir: default_systemd_user_dir(),
        }
    }
}

fn config_home() -> PathBuf {
    crate::util::config_dir()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_sway_config_path() -> PathBuf {
    config_home().join("sway/config")
}

fn default_systemd_user_dir() -> PathBuf {
    config_home().join("systemd/user")
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid bind address {value:?}: {source}")]
    Bind {
        value: String,
        #[source]
        source: std::net::AddrParseError,
    },
}

impl BootstrapConfig {
    /// Default path of the bootstrap config file.
    pub fn default_path() -> PathBuf {
        crate::util::config_dir().join("suede.toml")
    }

    /// Load from `path` (missing file means all defaults), then apply `SUEDE_*` overrides.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_path);
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str::<FileConfig>(&text).map_err(|source| ConfigError::Parse {
                    path: path.clone(),
                    source,
                })?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no bootstrap config file; using defaults");
                FileConfig::default()
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };

        let mut config = Self::default();

        if let Some(bind) = env_or(file.bind, "SUEDE_BIND") {
            config.bind = bind.parse().map_err(|source| ConfigError::Bind {
                value: bind.clone(),
                source,
            })?;
        }
        config.token = env_or(file.token, "SUEDE_TOKEN").filter(|t| !t.is_empty());
        if let Some(dir) = std::env::var("SUEDE_STATE_DIR")
            .ok()
            .filter(|v| !v.is_empty())
        {
            config.state_dir = PathBuf::from(dir);
        } else if let Some(dir) = file.state_dir {
            config.state_dir = dir;
        }
        if let Some(url) = env_or(file.docs_base_url, "SUEDE_DOCS_BASE_URL") {
            config.docs_base_url = url;
        }

        Ok(config)
    }

    /// True when a bearer token is configured, which also disables the web UI.
    pub fn auth_enabled(&self) -> bool {
        self.token.is_some()
    }

    /// Build a documentation URL for a health check.
    pub fn docs_url(&self, relative: &str) -> String {
        format!(
            "{}{}",
            self.docs_base_url.trim_end_matches('/'),
            if relative.starts_with('/') {
                relative.to_string()
            } else {
                format!("/{relative}")
            }
        )
    }

    /// Warn loudly about an unauthenticated, non-loopback deployment.
    pub fn log_security_posture(&self) {
        if self.auth_enabled() {
            tracing::info!("bearer token configured; reference web UI is disabled");
        } else if !self.bind.ip().is_loopback() {
            tracing::warn!(
                bind = %self.bind,
                "serving without authentication on a non-loopback address; \
                 set SUEDE_TOKEN if this network is not trusted"
            );
        }
    }
}

fn env_or(file_value: Option<String>, var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .or(file_value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let config = BootstrapConfig::load(Some(Path::new("/nonexistent/suede.toml"))).unwrap();
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert!(config.token.is_none());
    }

    #[test]
    fn docs_url_joins_without_double_slash() {
        let config = BootstrapConfig {
            docs_base_url: "https://example.com/".into(),
            ..Default::default()
        };
        assert_eq!(
            config.docs_url("troubleshooting/#sway"),
            "https://example.com/troubleshooting/#sway"
        );
    }

    #[test]
    fn file_values_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suede.toml");
        std::fs::write(&path, "bind = \"127.0.0.1:9000\"\ntoken = \"abc\"\n").unwrap();
        let config = BootstrapConfig::load(Some(&path)).unwrap();
        assert_eq!(config.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(config.token.as_deref(), Some("abc"));
        assert!(config.auth_enabled());
    }
}
