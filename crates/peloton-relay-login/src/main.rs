use std::io::{IsTerminal as _, stderr, stdin, stdout};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use peloton::account::AccountName;
use peloton::ids::PelotonId;
use peloton_relay_login::CliError;
use peloton_relay_login::output::TokenOutput;
use peloton_relay_login::post::{self, Code, PostError, SeedUrl};
use peloton_relay_login::session::Session;
use peloton_relay_login::{capture, check, scrub, spike};

/// macOS login helper that obtains Peloton tokens for the relay.
///
/// `login` prints the token set as JSON on stdout, or with `--account`,
/// `--post-to` and `--code` posts it to the relay; everything else goes to
/// stderr.
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(flatten)]
    destination: Destination,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Where `login` hands the token set over: stdout, or the relay's admin API
/// with all three options, which the relay's admin page shows as a command.
#[derive(Args)]
struct Destination {
    /// The relay account to sign in.
    #[arg(long, requires_all = ["post_to", "code"])]
    account: Option<AccountName>,
    /// The relay's seed URL: https, or http to a loopback address.
    #[arg(long, requires_all = ["account", "code"])]
    post_to: Option<SeedUrl>,
    /// The one-time code from the relay's admin page.
    #[arg(long, requires_all = ["account", "post_to"])]
    code: Option<Code>,
    /// Post without asking to confirm the relay's address first.
    #[arg(long, requires = "post_to")]
    yes: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Log in and print the token set, or post it to the relay (the default).
    Login(Destination),
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
    match run(cli.command.unwrap_or(Command::Login(cli.destination))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", peloton_relay_login::report(&error));
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<(), CliError> {
    match command {
        Command::Login(destination) => login(destination),
        Command::Check => Ok(check::run(&Session::new()?)?),
        Command::Capture { dir, workouts } => Ok(capture::run(&Session::new()?, &dir, workouts)?),
        Command::Spike { class, dir } => Ok(spike::run(&Session::new()?, &dir, &class)?),
        Command::Scrub { in_dir, out_dir } => Ok(scrub::run(&in_dir, &out_dir)?),
    }
}

fn login(destination: Destination) -> Result<(), CliError> {
    if let Destination {
        account: Some(account),
        post_to: Some(post_to),
        code: Some(code),
        yes,
    } = destination
    {
        // Before the sign-in window: a pasted command could name any host.
        if !yes {
            let input = stdin();
            if !input.is_terminal() {
                return Err(PostError::NoTerminal.into());
            }
            if !post::confirm(&post_to, &account, input.lock(), stderr())? {
                return Err(PostError::Declined.into());
            }
        }
        let session = Session::new()?;
        let tokens = session.login()?;
        eprintln!("Login succeeded; handing the token set to the relay.");
        let confirmation = session.post(&post_to, &account, &code, &tokens)?;
        eprintln!("{confirmation}");
        return Ok(());
    }
    let stdout = stdout();
    let is_terminal = stdout.is_terminal();
    let output = TokenOutput::new(stdout, is_terminal)?;
    let tokens = Session::new()?.login()?;
    output.write(&tokens)?;
    eprintln!("Login succeeded.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Cli, Command};

    const CODE: &str = "0123456789abcdef0123456789abcdef";
    const URL: &str = "https://relay.example.net/admin/seed";

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("peloton-relay-login").chain(args.iter().copied()))
    }

    #[test]
    fn post_options_come_all_together() {
        let cli = parse(&["--account", "bob", "--post-to", URL, "--code", CODE]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.destination.account.unwrap().to_string(), "bob");
        let cli = parse(&[
            "login",
            "--account",
            "bob",
            "--post-to",
            URL,
            "--code",
            CODE,
        ])
        .unwrap();
        assert!(matches!(cli.command, Some(Command::Login(ref d)) if d.code.is_some()));
        assert!(parse(&[]).unwrap().destination.post_to.is_none());
        for partial in [
            &["--account", "bob"][..],
            &["--post-to", URL][..],
            &["--code", CODE][..],
            &["--account", "bob", "--post-to", URL][..],
            &["--post-to", URL, "--code", CODE][..],
            &["login", "--account", "bob", "--code", CODE][..],
        ] {
            assert!(parse(partial).is_err(), "{partial:?}");
        }
    }

    #[test]
    fn post_options_are_validated() {
        for bad in [
            &["--account", "Bob", "--post-to", URL, "--code", CODE][..],
            &[
                "--account",
                "bob",
                "--post-to",
                "http://relay.example.net/admin/seed",
                "--code",
                CODE,
            ][..],
            &["--account", "bob", "--post-to", URL, "--code", "short"][..],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn yes_goes_with_the_post_options_only() {
        let cli = parse(&[
            "--account",
            "bob",
            "--post-to",
            URL,
            "--code",
            CODE,
            "--yes",
        ])
        .unwrap();
        assert!(cli.destination.yes);
        let cli = parse(&["--account", "bob", "--post-to", URL, "--code", CODE]).unwrap();
        assert!(!cli.destination.yes);
        assert!(parse(&["--yes"]).is_err());
        assert!(parse(&["login", "--yes"]).is_err());
    }

    #[test]
    fn post_options_belong_to_login_only() {
        assert!(
            parse(&[
                "check",
                "--account",
                "bob",
                "--post-to",
                URL,
                "--code",
                CODE
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "--account",
                "bob",
                "--post-to",
                URL,
                "--code",
                CODE,
                "check"
            ])
            .is_err()
        );
    }
}
