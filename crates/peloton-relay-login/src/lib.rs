//! macOS login helper that obtains Peloton tokens for the relay.

pub mod capture;
pub mod check;
pub mod output;
pub mod session;
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
