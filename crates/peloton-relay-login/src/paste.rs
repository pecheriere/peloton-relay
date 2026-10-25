//! Paste mode: the user logs in with their own browser and pastes the callback URL.

use url::Url;

use crate::LoginError;

/// Prints `authorize_url` to stderr and reads the callback URL from the
/// terminal without echo.
///
/// # Errors
///
/// Returns [`LoginError::Input`] when the terminal cannot be read.
pub fn read_callback(authorize_url: &Url) -> Result<String, LoginError> {
    eprintln!("Open this URL in a browser and log in:\n\n{authorize_url}\n");
    eprintln!(
        "Your browser will end up on a page that fails to load. Copy that page's full address."
    );
    let pasted = rpassword::prompt_password("Paste the callback URL (input is hidden): ")?;
    Ok(pasted.trim().to_owned())
}
