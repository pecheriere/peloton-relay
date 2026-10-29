use std::collections::HashMap;
use std::io::{self, IsTerminal as _};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use peloton::endpoints::API_BASE;
use peloton::oauth::{AUTH_DOMAIN, AuthClient};
use peloton_relay::account::{AccountName, Accounts};
use peloton_relay::api::{AccountContext, AccountRegistry, AppState, Clock, router};
use peloton_relay::config::{Config, http_client};
use peloton_relay::seed::{run_bind, run_seed};
use peloton_relay::tokens::{BACKGROUND_REFRESH_INTERVAL, DEFAULT_REFRESH_MARGIN, TokenManager};
use peloton_relay::upstream::{DEFAULT_MAX_IN_FLIGHT, Peloton};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::{self, JoinSet};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Peloton API relay server.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server.
    Serve,
    /// Read a token set as JSON from stdin, verify it with Peloton and store
    /// it in one account, bound to its Peloton user.
    Seed {
        /// The configured account to seed.
        #[arg(long)]
        account: AccountName,
        /// Replace the account's binding to another Peloton user.
        #[arg(long)]
        rebind: bool,
    },
    /// Bind one account to the Peloton user of the state file it already has.
    Bind {
        /// The configured account to bind.
        #[arg(long)]
        account: AccountName,
    },
    /// Exit 0 when the local server answers its liveness probe, else 1.
    Healthcheck,
}

/// Total time the healthcheck may take.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// How long shutdown waits for an in-flight refresh to be saved.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `seed` and `bind` wait for a refresh the server has in flight:
/// longer than one refresh request may take (30 s) plus saving its result.
const SEED_LOCK_WAIT: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve => serve().await,
        Command::Seed { account, rebind } => seed(&account, rebind).await,
        Command::Bind { account } => bind(account).await,
        Command::Healthcheck => healthcheck().await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<(), String> {
    let config = Config::from_env().map_err(|err| err.to_string())?;
    peloton_relay::logging::init().map_err(|err| format!("cannot set up logging: {err}"))?;
    peloton_relay::logging::install_panic_hook();

    let http = http_client().map_err(|err| format!("cannot build the HTTP client: {err}"))?;
    let auth_base = Url::parse(&format!("https://{AUTH_DOMAIN}")).map_err(|err| err.to_string())?;
    let api_base = Url::parse(API_BASE).map_err(|err| err.to_string())?;
    refuse_single_account_state(&config.accounts)?;
    let mut contexts = Vec::new();
    let mut managers = Vec::new();
    for account in config.accounts.names() {
        if let Err(err) = config.accounts.ensure_dir(account) {
            // Not fatal: the account stays unauthenticated, the others serve.
            tracing::error!(
                event = "account_dir_unusable",
                account = %account,
                error = %err,
                "cannot create the account directory"
            );
        }
        let tokens = Arc::new(TokenManager::new(
            account.clone(),
            config.accounts.state_file(account),
            AuthClient::new(http.clone(), auth_base.clone()),
            DEFAULT_REFRESH_MARGIN,
        ));
        let peloton = Arc::new(Peloton::new(
            http.clone(),
            api_base.clone(),
            Arc::clone(&tokens),
            DEFAULT_MAX_IN_FLIGHT,
        ));
        contexts.push(AccountContext::new(peloton));
        managers.push(tokens);
    }
    let accounts = AccountRegistry::new(contexts).map_err(|err| err.to_string())?;
    let app = router(AppState {
        accounts: Arc::new(accounts),
        clock: Clock::system(),
    });

    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|err| format!("cannot listen on {}: {err}", config.listen))?;
    let shutdown = CancellationToken::new();
    let signal = shutdown_signal(shutdown.clone())?;
    let mut refreshers = AccountTasks::default();
    for tokens in &managers {
        let account = tokens.account().clone();
        let tokens = Arc::clone(tokens);
        let shutdown = shutdown.clone();
        refreshers.spawn(account, async move {
            tokens
                .keep_fresh(BACKGROUND_REFRESH_INTERVAL, &shutdown)
                .await;
        });
    }
    let names = config
        .accounts
        .names()
        .iter()
        .map(AccountName::as_str)
        .collect::<Vec<_>>()
        .join(",");
    tracing::info!(listen = %config.listen, accounts = %names, "serving");
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(signal)
        .await;

    // Also reached when the server fails: stop refreshing, then let in-flight
    // refreshes save their rotated refresh tokens before exiting, all
    // accounts waiting concurrently.
    shutdown.cancel();
    refreshers.join_all("background refresh task failed").await;
    let mut drains = AccountTasks::default();
    for tokens in managers {
        drains.spawn(tokens.account().clone(), async move {
            if !tokens.drain(DRAIN_TIMEOUT).await {
                tracing::warn!(
                    event = "token_work_abandoned",
                    account = %tokens.account(),
                    "a token refresh was still running at shutdown"
                );
            }
        });
    }
    drains.join_all("token drain task failed").await;
    served.map_err(|err| format!("server failed: {err}"))?;
    tracing::info!("shut down");
    Ok(())
}

/// Tasks that each work for one account, so that a failed one can be logged
/// with the account it served.
#[derive(Default)]
struct AccountTasks {
    tasks: JoinSet<()>,
    accounts: HashMap<task::Id, AccountName>,
}

impl AccountTasks {
    fn spawn(&mut self, account: AccountName, work: impl Future<Output = ()> + Send + 'static) {
        let id = self.tasks.spawn(work).id();
        self.accounts.insert(id, account);
    }

    /// Waits for every task to end, logging `failure` with the account of
    /// each one that panicked or was cancelled.
    async fn join_all(mut self, failure: &'static str) {
        while let Some(joined) = self.tasks.join_next_with_id().await {
            match joined {
                Ok((id, ())) => {
                    self.accounts.remove(&id);
                }
                Err(err) => {
                    let account = self.accounts.remove(&err.id());
                    tracing::error!(
                        account = account.as_ref().map(tracing::field::display),
                        "{failure}"
                    );
                }
            }
        }
    }
}

/// Refuses to serve while the state file of the single-account releases is
/// still in the state directory: it would be silently ignored.
fn refuse_single_account_state(accounts: &Accounts) -> Result<(), String> {
    let legacy = accounts.legacy_state_file();
    match std::fs::symlink_metadata(&legacy) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("cannot inspect {}: {err}", legacy.display())),
        Ok(_) => Err(format!(
            "found {} from a single-account release; move it to {}/<account>/state.json \
             and run \"peloton-relay bind --account <account>\"",
            legacy.display(),
            accounts.root().display()
        )),
    }
}

/// Resolves on SIGTERM or Ctrl-C, cancelling `shutdown` first.
fn shutdown_signal(shutdown: CancellationToken) -> Result<impl Future<Output = ()>, String> {
    let mut term = signal(SignalKind::terminate())
        .map_err(|err| format!("cannot install the SIGTERM handler: {err}"))?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        shutdown.cancel();
    })
}

async fn seed(account: &AccountName, rebind: bool) -> Result<(), String> {
    let config = Config::from_env().map_err(|err| err.to_string())?;
    if io::stdin().is_terminal() {
        eprintln!("reading the token JSON from stdin; pipe it in, e.g. from peloton-relay-login");
    }
    let http = http_client().map_err(|err| format!("cannot build the HTTP client: {err}"))?;
    let api_base = Url::parse(API_BASE).map_err(|err| err.to_string())?;
    let seeded = run_seed(
        io::stdin().lock(),
        &config.accounts,
        account,
        rebind,
        &http,
        &api_base,
        SEED_LOCK_WAIT,
    )
    .await
    .map_err(|err| error_chain(&err))?;
    eprintln!("{seeded}");
    Ok(())
}

async fn bind(account: AccountName) -> Result<(), String> {
    let config = Config::from_env().map_err(|err| err.to_string())?;
    // Waiting for the locks blocks a thread; keep it off the async workers.
    let bound =
        tokio::task::spawn_blocking(move || run_bind(&config.accounts, &account, SEED_LOCK_WAIT))
            .await
            .map_err(|_| "the bind task did not complete".to_owned())?
            .map_err(|err| error_chain(&err))?;
    eprintln!("{bound}");
    Ok(())
}

/// `err` and each of its causes, joined by `: `, so that the operator sees
/// why (e.g. permission denied rather than corrupt). Seed and bind errors and
/// their causes never carry tokens, usernames or file contents.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut shown = err.to_string();
    let mut cause = err.source();
    while let Some(err) = cause {
        shown.push_str(": ");
        shown.push_str(&err.to_string());
        cause = err.source();
    }
    shown
}

async fn healthcheck() -> Result<(), String> {
    let config = Config::from_env().map_err(|err| err.to_string())?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HEALTHCHECK_TIMEOUT)
        .build()
        .map_err(|err| format!("cannot build the HTTP client: {err}"))?;
    let response = client
        .get(config.healthcheck_url())
        .send()
        .await
        .map_err(|_| "the server is unreachable".to_owned())?;
    if response.status() == reqwest::StatusCode::OK {
        Ok(())
    } else {
        Err(format!("unhealthy: status {}", response.status().as_u16()))
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::MakeWriter;

    use super::AccountTasks;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl MakeWriter<'_> for Buffer {
        type Writer = Self;

        fn make_writer(&self) -> Self {
            self.clone()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_account_task_is_logged_with_its_account() {
        let buffer = Buffer::default();
        let subscriber = peloton_relay::logging::subscriber(buffer.clone(), EnvFilter::new("info"));
        let _default = tracing::subscriber::set_default(subscriber);
        let mut tasks = AccountTasks::default();
        tasks.spawn("alice".parse().unwrap(), async {});
        tasks.spawn("bob".parse().unwrap(), async {
            panic!("the task of bob fails");
        });

        tasks.join_all("test task failed").await;

        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 1, "{output}");
        assert_eq!(lines[0]["message"], "test task failed");
        assert_eq!(lines[0]["account"], "bob");
    }
}
