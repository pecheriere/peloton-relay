use std::io::{IsTerminal as _, stdout};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use peloton::ids::PelotonId;
use peloton_relay_login::CliError;
use peloton_relay_login::output::TokenOutput;
use peloton_relay_login::session::Session;
use peloton_relay_login::{capture, check, scrub, spike};

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
    /// Log in and round-trip the schedule and Stack operations on the account,
    /// restoring both and saving every raw response.
    Spike {
        /// Ride id (32 lowercase hex) of the on-demand class to use.
        #[arg(long)]
        class: PelotonId,
        /// Directory to write into: outside the git work tree, or `<work tree>/captures`.
        #[arg(long, default_value = "captures")]
        dir: PathBuf,
    },
    /// Scrub captures into fixtures that are safe to publish (no login, no network).
    Scrub {
        /// Directory holding the raw captures.
        in_dir: PathBuf,
        /// Directory to write the scrubbed files into.
        out_dir: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command.unwrap_or(Command::Login)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", error.report());
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<(), CliError> {
    match command {
        Command::Login => login(),
        Command::Check => Ok(check::run(&Session::new()?)?),
        Command::Capture { dir, workouts } => Ok(capture::run(&Session::new()?, &dir, workouts)?),
        Command::Spike { class, dir } => Ok(spike::run(&Session::new()?, &dir, &class)?),
        Command::Scrub { in_dir, out_dir } => Ok(scrub::run(&in_dir, &out_dir)?),
    }
}

fn login() -> Result<(), CliError> {
    let stdout = stdout();
    let is_terminal = stdout.is_terminal();
    let output = TokenOutput::new(stdout, is_terminal)?;
    let tokens = Session::new()?.login()?;
    output.write(&tokens)?;
    eprintln!("Login succeeded.");
    Ok(())
}
