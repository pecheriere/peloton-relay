use std::io::{IsTerminal as _, stdout};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use peloton::oauth::{AUTH_DOMAIN, AuthClient, AuthRequest, parse_callback};
use peloton_relay_login::output::write_tokens;
use peloton_relay_login::paste;
#[cfg(target_os = "macos")]
use peloton_relay_login::window;
use url::Url;

/// macOS login helper that obtains Peloton tokens for the relay.
///
/// Prints the token set as JSON on stdout; everything else goes to stderr.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Log in and print the token set (the default).
    Login {
        /// Log in with your own browser and paste the callback URL instead of
        /// opening a window.
        #[arg(long)]
        paste: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paste = match cli.command {
        Some(Command::Login { paste }) => paste,
        None => false,
    };
    match run(paste) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(paste: bool) -> Result<(), String> {
    if stdout().is_terminal() {
        return Err("stdout is a terminal; pipe or redirect it to receive the tokens".to_owned());
    }
    let auth_base =
        Url::parse(&format!("https://{AUTH_DOMAIN}")).map_err(|e| format!("auth url: {e}"))?;
    let request = AuthRequest::generate();
    let authorize_url = request.authorize_url(&auth_base);

    let callback = if paste {
        paste::read_callback(&authorize_url)
    } else {
        capture_in_window(&authorize_url)
    }
    .map_err(|e| e.to_string())?;

    let code = parse_callback(&callback, &request.state).map_err(|e| e.to_string())?;

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "could not build the HTTP client".to_owned())?;
    let client = AuthClient::new(http, auth_base);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let tokens = runtime
        .block_on(client.exchange_code(&code, &request))
        .map_err(|e| e.to_string())?;

    write_tokens(&tokens, stdout(), false).map_err(|e| e.to_string())?;
    eprintln!("Login succeeded.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn capture_in_window(authorize_url: &Url) -> Result<String, peloton_relay_login::LoginError> {
    eprintln!("Opening the Peloton login window...");
    window::capture_callback(authorize_url)
}

#[cfg(not(target_os = "macos"))]
fn capture_in_window(_authorize_url: &Url) -> Result<String, peloton_relay_login::LoginError> {
    Err(peloton_relay_login::LoginError::Window(
        "window mode needs macOS; use --paste".to_owned(),
    ))
}
