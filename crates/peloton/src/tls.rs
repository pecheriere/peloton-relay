//! The trust anchors every HTTPS client of the workspace uses.

use reqwest::{Certificate, ClientBuilder};

/// The Mozilla root certificates compiled into the binary.
///
/// reqwest otherwise verifies through the platform, which the `scratch`
/// release image cannot provide; passing these to
/// [`reqwest::ClientBuilder::tls_certs_only`] keeps verification independent
/// of any system certificate store.
///
/// # Errors
///
/// When one of the compiled-in certificates cannot be parsed.
fn root_certificates() -> reqwest::Result<Vec<Certificate>> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| Certificate::from_der(der))
        .collect()
}

/// A client builder that trusts only the compiled-in root certificates.
///
/// Every HTTP client in the workspace starts from this builder; clippy
/// rejects `reqwest::Client::builder` and `reqwest::Client::new` elsewhere.
///
/// # Errors
///
/// When one of the compiled-in certificates cannot be parsed.
pub fn client_builder() -> reqwest::Result<ClientBuilder> {
    // The one place the roots are applied, which is why the lint is lifted.
    #[allow(clippy::disallowed_methods)]
    let builder = reqwest::Client::builder();
    Ok(builder.tls_certs_only(root_certificates()?))
}

#[cfg(test)]
mod tests {
    use super::{client_builder, root_certificates};

    #[test]
    fn the_compiled_in_roots_load_into_a_client() {
        let roots = root_certificates().expect("roots parse");
        assert!(roots.len() > 100, "only {} roots", roots.len());
        client_builder()
            .expect("roots parse")
            .build()
            .expect("client builds from the compiled-in roots alone");
    }
}
