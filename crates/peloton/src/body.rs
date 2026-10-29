//! Bounded reading of response bodies, shared by every call to Peloton and
//! its token endpoint.

/// Most bytes read from one response body.
///
/// The largest legitimate answers are workout metrics and library pages,
/// about 40 KiB in the scrubbed fixtures and well under 1 MiB for a long
/// workout; this leaves ample headroom while bounding the memory a faulty or
/// hostile server can make the caller hold.
pub const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Why a response body could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BodyError {
    /// The body is larger than [`MAX_RESPONSE_BODY_BYTES`].
    #[error("response body is larger than {MAX_RESPONSE_BODY_BYTES} bytes")]
    TooLarge,
    /// The connection failed while the body was being read.
    #[error("reading the response body failed")]
    Transport {
        /// Whether the request's total timeout elapsed.
        timeout: bool,
    },
}

/// The body of `response`, read in chunks so that it never holds more than
/// [`MAX_RESPONSE_BODY_BYTES`].
///
/// A declared `Content-Length` over the limit is refused before any of the
/// body is read; a body without one, or understating it, is cut off at the
/// limit.
///
/// # Errors
///
/// [`BodyError`] when the body is too large or cannot be read.
pub async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, BodyError> {
    let declared_too_large = response
        .content_length()
        .is_some_and(|length| usize::try_from(length).is_ok_and(|n| n > MAX_RESPONSE_BODY_BYTES));
    if declared_too_large {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| BodyError::Transport {
        timeout: err.is_timeout(),
    })? {
        if chunk.len() > MAX_RESPONSE_BODY_BYTES - body.len() {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
