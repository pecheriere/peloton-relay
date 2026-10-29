//! The admin page: every account's credential health, and revoking or
//! renewing an account's sign-in.

pub mod codes;
pub mod csrf;

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}
