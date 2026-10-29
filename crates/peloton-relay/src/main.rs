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
    let mut refreshers = tokio::task::JoinSet::new();
    for tokens in &managers {
        let tokens = Arc::clone(tokens);
        let shutdown = shutdown.clone();
        refreshers.spawn(async move {
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
    while let Some(joined) = refreshers.join_next().await {
        if joined.is_err() {
            tracing::error!("background refresh task failed");
        }
    }
    let mut drains = tokio::task::JoinSet::new();
    for tokens in managers {
        drains.spawn(async move {
            if !tokens.drain(DRAIN_TIMEOUT).await {
                tracing::warn!(
                    event = "token_work_abandoned",
                    account = %tokens.account(),
                    "a token refresh was still running at shutdown"
                );
            }
        });
    }
    while let Some(joined) = drains.join_next().await {
        if joined.is_err() {
            tracing::error!("token drain task failed");
        }
    }
    served.map_err(|err| format!("server failed: {err}"))?;
    tracing::info!("shut down");
    Ok(())
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
    .map_err(|err| err.to_string())?;
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
            .map_err(|err| err.to_string())?;
    eprintln!("{bound}");
    Ok(())
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
