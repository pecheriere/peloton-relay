use std::io::{IsTerminal as _, stdout};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use peloton_relay_login::output::write_tokens;
use peloton_relay_login::session::Session;
use peloton_relay_login::{capture, check};

/// macOS login helper that obtains Peloton tokens for the relay.
///
/// `login` prints the token set as JSON on stdout; everything else goes to stderr.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Log in and print the token set (the default).
    Login,
    /// Log in, call /me, refresh once and report non-secret facts.
    Check,
    /// Log in and save raw Peloton responses for building fixtures.
    Capture {
        /// Directory to write into: outside the git work tree, or `<work tree>/captures`.
        #[arg(long)]
        dir: PathBuf,
        /// How many recent workouts to capture.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100))]
        workouts: u32,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command.unwrap_or(Command::Login)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Login => login(),
        Command::Check => check::run(&Session::new()?),
        Command::Capture { dir, workouts } => capture::run(&Session::new()?, &dir, workouts),
    }
}

fn login() -> Result<(), String> {
    if stdout().is_terminal() {
        return Err("stdout is a terminal; pipe or redirect it to receive the tokens".to_owned());
    }
    let tokens = Session::new()?.login()?;
    write_tokens(&tokens, stdout(), false).map_err(|e| e.to_string())?;
    eprintln!("Login succeeded.");
    Ok(())
}
