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

/// Writes `set` as one line of JSON to `out`.
///
/// # Errors
///
/// Returns [`OutputError::RefusingTerminal`] when `out_is_terminal` is set, and
/// other variants when serializing or writing fails.
pub fn write_tokens(
    set: &TokenSet,
    mut out: impl Write,
    out_is_terminal: bool,
) -> Result<(), OutputError> {
    if out_is_terminal {
        return Err(OutputError::RefusingTerminal);
    }
    serde_json::to_writer(&mut out, set)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
