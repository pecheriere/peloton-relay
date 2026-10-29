//! macOS login helper that obtains Peloton tokens for the relay.

pub mod capture;
pub mod check;
pub mod output;
pub mod scrub;
pub mod session;
pub mod spike;
#[cfg(target_os = "macos")]
pub mod window;

/// Why a login attempt failed. Never carries tokens or callback URLs.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The user closed the login window before the callback arrived.
    #[error("login cancelled")]
    Cancelled,
    /// The login window could not be created or run.
    #[error("login window failed: {0}")]
    Window(String),
}

/// Why a subcommand failed, as reported by the CLI. Never carries tokens.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CliError {
    /// The token set could not be handed over on stdout.
    #[error(transparent)]
    Output(#[from] output::OutputError),
    /// The session could not be set up or the login failed.
    #[error(transparent)]
    Session(#[from] session::SessionError),
    /// `check` failed.
    #[error(transparent)]
    Check(#[from] check::CheckError),
    /// `capture` failed.
    #[error(transparent)]
    Capture(#[from] capture::CaptureError),
    /// `spike` failed.
    #[error(transparent)]
    Spike(#[from] spike::SpikeError),
    /// `scrub` failed.
    #[error(transparent)]
    Scrub(#[from] scrub::ScrubError),
}

/// `error` and its causes on one line, outermost first, joined with ": ".
#[must_use]
pub fn report(error: &(dyn std::error::Error + 'static)) -> String {
    let mut report = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        report.push_str(": ");
        report.push_str(&cause.to_string());
        source = cause.source();
    }
    report
}

#[cfg(test)]
mod tests {
    use super::{CliError, report};
    use crate::output::{OutputError, TokenOutput};

    #[test]
    fn terminal_refusal_reports_the_output_message_unchanged() {
        let error = TokenOutput::new(Vec::new(), true).expect_err("terminal refused");
        assert_eq!(
            report(&CliError::from(error)),
            "refusing to print tokens to a terminal; pipe or redirect stdout"
        );
    }

    #[test]
    fn write_failure_reports_the_io_cause_after_the_message() {
        let cause = std::io::Error::other("disk full");
        let error = CliError::from(OutputError::from(cause));
        assert_eq!(report(&error), "could not write tokens: disk full");
    }

    #[test]
    fn output_failure_keeps_its_source_chain() {
        let error = CliError::from(OutputError::from(std::io::Error::other("closed")));
        let source = std::error::Error::source(&error).expect("source of the output error");
        assert_eq!(source.to_string(), "closed");
    }
}
