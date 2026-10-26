use std::io::{self, IsTerminal as _};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use peloton::endpoints::API_BASE;
use peloton::oauth::{AUTH_DOMAIN, AuthClient};
use peloton_relay::api::{AppState, router};
use peloton_relay::config::{Config, http_client};
use peloton_relay::seed::run_seed;
use peloton_relay::tokens::{BACKGROUND_REFRESH_INTERVAL, DEFAULT_REFRESH_MARGIN, TokenManager};
use peloton_relay::upstream::{DEFAULT_MAX_IN_FLIGHT, Peloton};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Read-only Peloton API relay server.
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
    /// Read a token set as JSON from stdin, verify it with Peloton and store it.
    Seed,
    /// Exit 0 when the local server answers its liveness probe, else 1.
    Healthcheck,
}

/// Total time the healthcheck may take.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// How long shutdown waits for an in-flight refresh to be saved.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve => serve().await,
        Command::Seed => seed().await,
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

    let http = http_client().map_err(|err| format!("cannot build the HTTP client: {err}"))?;
    let auth_base = Url::parse(&format!("https://{AUTH_DOMAIN}")).map_err(|err| err.to_string())?;
    let api_base = Url::parse(API_BASE).map_err(|err| err.to_string())?;
    let tokens = Arc::new(TokenManager::new(
        config.state_file(),
        AuthClient::new(http.clone(), auth_base),
        DEFAULT_REFRESH_MARGIN,
    ));
    let peloton = Arc::new(Peloton::new(
        http,
        api_base,
        Arc::clone(&tokens),
        DEFAULT_MAX_IN_FLIGHT,
    ));
    let app = router(AppState {
        peloton,
        tokens: Arc::clone(&tokens),
    });

    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|err| format!("cannot listen on {}: {err}", config.listen))?;
    let shutdown = CancellationToken::new();
    let signal = shutdown_signal(shutdown.clone())?;
    let refresher = tokio::spawn({
        let tokens = Arc::clone(&tokens);
        let shutdown = shutdown.clone();
        async move {
            tokens
                .keep_fresh(BACKGROUND_REFRESH_INTERVAL, &shutdown)
                .await;
        }
    });
    tracing::info!(listen = %config.listen, "serving");
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(signal)
        .await;

    // Also reached when the server fails: stop refreshing, then let an
    // in-flight refresh save the rotated refresh token before exiting.
    shutdown.cancel();
    if refresher.await.is_err() {
        tracing::error!("background refresh task failed");
    }
    if !tokens.drain(DRAIN_TIMEOUT).await {
        tracing::warn!(
            event = "token_work_abandoned",
            "a token refresh was still running at shutdown"
        );
    }
    served.map_err(|err| format!("server failed: {err}"))?;
    tracing::info!("shut down");
    Ok(())
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

async fn seed() -> Result<(), String> {
    let config = Config::from_env().map_err(|err| err.to_string())?;
    if io::stdin().is_terminal() {
        eprintln!("reading the token JSON from stdin; pipe it in, e.g. from peloton-relay-login");
    }
    let http = http_client().map_err(|err| format!("cannot build the HTTP client: {err}"))?;
    let api_base = Url::parse(API_BASE).map_err(|err| err.to_string())?;
    let username = run_seed(io::stdin().lock(), &config.state_file(), &http, &api_base)
        .await
        .map_err(|err| err.to_string())?;
    eprintln!("seeded user {username}");
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
    let url = format!("http://127.0.0.1:{}/healthz", config.listen.port());
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "the server is unreachable".to_owned())?;
    if response.status() == reqwest::StatusCode::OK {
        Ok(())
    } else {
        Err(format!("unhealthy: status {}", response.status().as_u16()))
    }
}
