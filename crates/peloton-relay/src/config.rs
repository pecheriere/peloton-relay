//! Process configuration from the environment, and the shared HTTP client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::store::StateFile;

/// Environment variable holding the listen address.
pub const LISTEN_VAR: &str = "PELOTON_RELAY_LISTEN";
/// Environment variable holding the state directory.
pub const STATE_DIR_VAR: &str = "PELOTON_RELAY_STATE_DIR";

const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
const DEFAULT_STATE_DIR: &str = "/state";
const STATE_FILE_NAME: &str = "state.json";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// An environment variable has an unusable value.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The listen address is not `host:port`.
    #[error("{LISTEN_VAR} must be an address like 0.0.0.0:8080")]
    Listen,
    /// The state directory is empty or not valid unicode.
    #[error("{STATE_DIR_VAR} must be a non-empty path")]
    StateDir,
}

/// Settings shared by all commands.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to; its port is also the healthcheck's.
    pub listen: SocketAddr,
    /// Directory holding the state file.
    pub state_dir: PathBuf,
}

impl Config {
    /// Reads the configuration from the process environment.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when a variable is set to an unusable value.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// Reads the configuration through `lookup`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when a variable is set to an unusable value.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self, ConfigError> {
        let listen = match lookup(LISTEN_VAR) {
            None => DEFAULT_LISTEN.to_owned(),
            Some(value) => value.into_string().map_err(|_| ConfigError::Listen)?,
        }
        .parse()
        .map_err(|_| ConfigError::Listen)?;
        let state_dir = match lookup(STATE_DIR_VAR) {
            None => PathBuf::from(DEFAULT_STATE_DIR),
            Some(value) if value.is_empty() => return Err(ConfigError::StateDir),
            Some(value) => PathBuf::from(value),
        };
        Ok(Self { listen, state_dir })
    }

    /// The state file inside the state directory.
    #[must_use]
    pub fn state_file(&self) -> StateFile {
        StateFile {
            path: self.state_dir.join(STATE_FILE_NAME),
        }
    }
}

/// Builds the HTTP client for Peloton and Auth0: no redirects (a redirect must
/// never carry a bearer token elsewhere), bounded connect and total time.
///
/// # Errors
///
/// When the TLS backend cannot be initialised.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Config, ConfigError};

    fn config(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_lookup(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    #[test]
    fn defaults() {
        let config = config(&[]).unwrap();
        assert_eq!(config.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.state_file().path.to_str(), Some("/state/state.json"));
    }

    #[test]
    fn overrides() {
        let config = config(&[
            ("PELOTON_RELAY_LISTEN", "127.0.0.1:9000"),
            ("PELOTON_RELAY_STATE_DIR", "/tmp/x"),
        ])
        .unwrap();
        assert_eq!(config.listen.port(), 9000);
        assert_eq!(config.state_file().path.to_str(), Some("/tmp/x/state.json"));
    }

    #[test]
    fn rejects_bad_values() {
        assert!(matches!(
            config(&[("PELOTON_RELAY_LISTEN", "nonsense")]),
            Err(ConfigError::Listen)
        ));
        assert!(matches!(
            config(&[("PELOTON_RELAY_STATE_DIR", "")]),
            Err(ConfigError::StateDir)
        ));
    }
}
