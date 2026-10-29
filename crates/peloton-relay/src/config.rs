//! Process configuration from the environment, and the shared HTTP client.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use peloton::hostname::is_dns_name;
use url::{Host, Url};

use crate::account::{AccountName, Accounts};

/// Environment variable holding the listen address.
pub const LISTEN_VAR: &str = "PELOTON_RELAY_LISTEN";
/// Environment variable holding the state directory.
pub const STATE_DIR_VAR: &str = "PELOTON_RELAY_STATE_DIR";
/// Environment variable holding the comma-separated account names.
pub const ACCOUNTS_VAR: &str = "PELOTON_RELAY_ACCOUNTS";
/// Environment variable holding the relay's externally reachable origin.
pub const PUBLIC_URL_VAR: &str = "PELOTON_RELAY_PUBLIC_URL";

const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
const DEFAULT_STATE_DIR: &str = "/state";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// An environment variable has an unusable value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The listen address is not `host:port`.
    #[error("{LISTEN_VAR} must be an address like 0.0.0.0:8080")]
    Listen,
    /// The state directory is empty or not valid unicode.
    #[error("{STATE_DIR_VAR} must be a non-empty path")]
    StateDir,
    /// The account list is unset, empty, malformed or has a repeated name.
    #[error(
        "{ACCOUNTS_VAR} must list one or more distinct account names (1 to 16 lowercase ASCII letters), comma-separated"
    )]
    Accounts,
    /// The public URL is not an `https` origin.
    #[error(
        "{PUBLIC_URL_VAR} must be an https origin like https://relay.example.net (a DNS name or an IP address), without credentials, path, query or fragment"
    )]
    PublicUrl,
}

/// Settings shared by all commands.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to, which the healthcheck probes.
    pub listen: SocketAddr,
    /// The accounts this relay serves, under the state directory.
    pub accounts: Accounts,
    /// The origin the deployment's gateway serves the relay at, which the
    /// admin page puts in the renew command; renewing from the page is off
    /// when unset.
    pub public_url: Option<Url>,
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
        let names = lookup(ACCOUNTS_VAR)
            .and_then(|value| value.into_string().ok())
            .ok_or(ConfigError::Accounts)?
            .split(',')
            .map(str::parse::<AccountName>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ConfigError::Accounts)?;
        let accounts = Accounts::new(state_dir, names).map_err(|_| ConfigError::Accounts)?;
        let public_url = lookup(PUBLIC_URL_VAR)
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| ConfigError::PublicUrl)
                    .and_then(|text| public_origin(&text))
            })
            .transpose()?;
        Ok(Self {
            listen,
            accounts,
            public_url,
        })
    }

    /// URL of the liveness probe of a server bound to [`Self::listen`]: the
    /// listen address itself, or loopback when it binds every address.
    ///
    /// An IPv6 zone index is dropped; probing a link-local address needs the
    /// server to also listen on loopback or a routable address.
    #[must_use]
    pub fn healthcheck_url(&self) -> String {
        let port = self.listen.port();
        match self.listen.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => {
                format!("http://{}:{port}/healthz", Ipv4Addr::LOCALHOST)
            }
            IpAddr::V4(ip) => format!("http://{ip}:{port}/healthz"),
            IpAddr::V6(ip) if ip.is_unspecified() => {
                format!("http://[{}]:{port}/healthz", Ipv6Addr::LOCALHOST)
            }
            IpAddr::V6(ip) => format!("http://[{ip}]:{port}/healthz"),
        }
    }
}

/// `text` as an `https` origin: a host and optional port, nothing else. The
/// admin routes sit at the root of that origin.
///
/// The host must be an IP address or a DNS name of letters, digits and
/// hyphens (after IDNA), since the admin page puts it in a command to paste
/// into a shell.
fn public_origin(text: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(text).map_err(|_| ConfigError::PublicUrl)?;
    let plain_host = match url.host() {
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => true,
        Some(Host::Domain(name)) => is_dns_name(name),
        None => false,
    };
    let origin_only = url.scheme() == "https"
        && plain_host
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none();
    if origin_only {
        Ok(url)
    } else {
        Err(ConfigError::PublicUrl)
    }
}

/// Builds the HTTP client for Peloton and Auth0: no redirects (a redirect must
/// never carry a bearer token elsewhere), bounded connect and total time.
///
/// # Errors
///
/// When the TLS backend cannot be initialised.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    peloton::tls::client_builder()?
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Config, ConfigError};
    use crate::account::AccountName;

    fn alice() -> AccountName {
        "alice".parse().unwrap()
    }

    fn config(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_lookup(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    #[test]
    fn defaults() {
        let config = config(&[("PELOTON_RELAY_ACCOUNTS", "alice")]).unwrap();
        assert_eq!(config.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.public_url, None);
        assert_eq!(
            config.accounts.state_file(&alice()).path.to_str(),
            Some("/state/alice/state.json")
        );
    }

    #[test]
    fn overrides() {
        let config = config(&[
            ("PELOTON_RELAY_LISTEN", "127.0.0.1:9000"),
            ("PELOTON_RELAY_STATE_DIR", "/tmp/x"),
            ("PELOTON_RELAY_ACCOUNTS", "alice"),
        ])
        .unwrap();
        assert_eq!(config.listen.port(), 9000);
        assert_eq!(
            config.accounts.state_file(&alice()).path.to_str(),
            Some("/tmp/x/alice/state.json")
        );
    }

    #[test]
    fn healthcheck_probes_the_listen_address() {
        let url = |listen: &str| {
            config(&[
                ("PELOTON_RELAY_LISTEN", listen),
                ("PELOTON_RELAY_ACCOUNTS", "alice"),
            ])
            .unwrap()
            .healthcheck_url()
        };
        assert_eq!(url("0.0.0.0:8080"), "http://127.0.0.1:8080/healthz");
        assert_eq!(url("[::]:8080"), "http://[::1]:8080/healthz");
        assert_eq!(url("127.0.0.1:9000"), "http://127.0.0.1:9000/healthz");
        assert_eq!(url("192.0.2.7:8080"), "http://192.0.2.7:8080/healthz");
        assert_eq!(url("[::1]:8080"), "http://[::1]:8080/healthz");
        assert_eq!(url("[2001:db8::7]:81"), "http://[2001:db8::7]:81/healthz");
        assert_eq!(url("[fe80::1%2]:8080"), "http://[fe80::1]:8080/healthz");
    }

    #[test]
    fn rejects_bad_values() {
        assert!(matches!(
            config(&[
                ("PELOTON_RELAY_LISTEN", "nonsense"),
                ("PELOTON_RELAY_ACCOUNTS", "alice"),
            ]),
            Err(ConfigError::Listen)
        ));
        assert!(matches!(
            config(&[
                ("PELOTON_RELAY_STATE_DIR", ""),
                ("PELOTON_RELAY_ACCOUNTS", "alice"),
            ]),
            Err(ConfigError::StateDir)
        ));
    }

    #[test]
    fn config_reads_an_https_public_origin() {
        for (given, kept) in [
            ("https://relay.example.net", "https://relay.example.net/"),
            ("https://relay.example.net/", "https://relay.example.net/"),
            (
                "https://relay.example.net:8443",
                "https://relay.example.net:8443/",
            ),
            ("https://192.0.2.7", "https://192.0.2.7/"),
            ("https://[2001:db8::7]", "https://[2001:db8::7]/"),
            ("https://Relay.Example.NET", "https://relay.example.net/"),
            (
                "https://xn--rly-0ma.example.net",
                "https://xn--rly-0ma.example.net/",
            ),
            (
                "https://r\u{e9}lay.example.net",
                "https://xn--rlay-bpa.example.net/",
            ),
        ] {
            let config = config(&[
                ("PELOTON_RELAY_ACCOUNTS", "alice"),
                ("PELOTON_RELAY_PUBLIC_URL", given),
            ])
            .unwrap();
            assert_eq!(config.public_url.unwrap().as_str(), kept, "{given}");
        }
    }

    #[test]
    fn config_rejects_a_public_url_that_is_not_an_https_origin() {
        for bad in [
            "",
            "relay.example.net",
            "http://relay.example.net",
            "ftp://relay.example.net",
            "https://relay.example.net/relay",
            "https://relay.example.net/admin/seed",
            "https://relay.example.net/?a=1",
            "https://relay.example.net/#top",
            "https://user@relay.example.net",
            "https://user:pass@relay.example.net",
            "https://",
            "https://a$(id).example",
            "https://a;id.example",
            "https://a`id`.example",
            "https://a&b.example",
            "https://a'b.example",
            "https://a{b}.example",
            "https://a*b.example",
            "https://a!b.example",
            "https://-relay.example.net",
            "https://relay-.example.net",
            "https://relay..example.net",
            "https://relay.example.net.",
        ] {
            assert!(
                matches!(
                    config(&[
                        ("PELOTON_RELAY_ACCOUNTS", "alice"),
                        ("PELOTON_RELAY_PUBLIC_URL", bad),
                    ]),
                    Err(ConfigError::PublicUrl)
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn config_reads_the_account_list() {
        let config = config(&[("PELOTON_RELAY_ACCOUNTS", "alice,bob")]).unwrap();
        let bob: AccountName = "bob".parse().unwrap();
        assert_eq!(config.accounts.names(), [alice(), bob]);
        assert_eq!(
            config.accounts.state_file(&alice()).path.to_str(),
            Some("/state/alice/state.json")
        );
    }

    #[test]
    fn config_rejects_bad_account_lists() {
        assert!(matches!(config(&[]), Err(ConfigError::Accounts)));
        for bad in [
            "",
            "alice,",
            ",alice",
            "alice,,bob",
            "alice, bob",
            "alice,alice",
            "alice,Bob",
        ] {
            assert!(
                matches!(
                    config(&[("PELOTON_RELAY_ACCOUNTS", bad)]),
                    Err(ConfigError::Accounts)
                ),
                "{bad:?}"
            );
        }
    }
}
