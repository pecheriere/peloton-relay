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
    /// A subcommand failed; the message holds no secrets.
    #[error("{0}")]
    Command(String),
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self::Command(message)
    }
}

impl CliError {
    /// The error and its causes on one line, outermost first.
    #[must_use]
    pub fn report(&self) -> String {
        let mut report = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            report.push_str(": ");
            report.push_str(&cause.to_string());
            source = cause.source();
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::CliError;
    use crate::output::{OutputError, TokenOutput};

    #[test]
    fn command_failure_reports_its_message_unchanged() {
        let error = CliError::from("unexpected /me response shape".to_owned());
        assert_eq!(error.report(), "unexpected /me response shape");
    }

    #[test]
    fn terminal_refusal_reports_the_output_message_unchanged() {
        let error = TokenOutput::new(Vec::new(), true).expect_err("terminal refused");
        assert_eq!(
            CliError::from(error).report(),
            "refusing to print tokens to a terminal; pipe or redirect stdout"
        );
    }

    #[test]
    fn write_failure_reports_the_io_cause_after_the_message() {
        let cause = std::io::Error::other("disk full");
        let error = CliError::from(OutputError::from(cause));
        assert_eq!(error.report(), "could not write tokens: disk full");
    }

    #[test]
    fn output_failure_keeps_its_source_chain() {
        let error = CliError::from(OutputError::from(std::io::Error::other("closed")));
        let source = std::error::Error::source(&error).expect("source of the output error");
        assert_eq!(source.to_string(), "closed");
    }
}
