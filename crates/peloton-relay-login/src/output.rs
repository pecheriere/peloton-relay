//! Writing the token set to stdout.

use std::io::Write;

use peloton::oauth::TokenSet;

/// Failure to hand the token set over.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    /// Stdout is a terminal; tokens must be piped, never displayed.
    #[error("refusing to print tokens to a terminal; pipe or redirect stdout")]
    RefusingTerminal,
    /// Serializing the token set failed.
    #[error("could not serialize tokens")]
    Serialize(#[from] serde_json::Error),
    /// Writing to the output failed.
    #[error("could not write tokens")]
    Write(#[from] std::io::Error),
}

/// Where the token set goes: anything but a terminal.
///
/// Created before signing in, so a terminal is refused before any tokens
/// exist rather than after the user has logged in.
#[derive(Debug)]
pub struct TokenOutput<W> {
    out: W,
}

impl<W: Write> TokenOutput<W> {
    /// Accepts `out` unless `out_is_terminal`.
    ///
    /// # Errors
    ///
    /// [`OutputError::RefusingTerminal`] when `out_is_terminal` is set.
    pub fn new(out: W, out_is_terminal: bool) -> Result<Self, OutputError> {
        if out_is_terminal {
            return Err(OutputError::RefusingTerminal);
        }
        Ok(Self { out })
    }

    /// Writes `set` as one line of JSON and flushes.
    ///
    /// # Errors
    ///
    /// When serializing or writing fails.
    pub fn write(mut self, set: &TokenSet) -> Result<(), OutputError> {
        serde_json::to_writer(&mut self.out, set)?;
        self.out.write_all(b"\n")?;
        self.out.flush()?;
        Ok(())
    }
}
