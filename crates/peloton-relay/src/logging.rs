//! JSON logging to stdout.

use std::io;

use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Builds the production subscriber writing to `writer`.
///
/// Event fields (such as `event="peloton_auth_failed"`) are top-level JSON
/// keys, where log alert rules match them. `filter` is an `EnvFilter` directive.
pub fn subscriber<W>(writer: W, filter: EnvFilter) -> impl Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(writer)
        .with_env_filter(filter)
        .finish()
}

/// Installs the production subscriber on stdout; `RUST_LOG` overrides the
/// default level `info`.
///
/// # Errors
///
/// When a global subscriber is already installed.
pub fn init() -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing::subscriber::set_global_default(subscriber(io::stdout, filter))
}

/// Replaces the default panic hook, which prints the panic's payload to
/// stderr, with one logging an error event `panic` with only the source
/// location: a payload could hold request or upstream data.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(
            event = "panic",
            location = info.location().map(tracing::field::display),
            "a thread panicked"
        );
    }));
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl MakeWriter<'_> for Buffer {
        type Writer = Self;

        fn make_writer(&self) -> Self {
            self.clone()
        }
    }

    #[test]
    fn event_field_is_top_level() {
        let buffer = Buffer::default();
        let subscriber = super::subscriber(buffer.clone(), EnvFilter::new("info"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(event = "peloton_auth_failed", "auth failed");
        });
        let line = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let json: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(json["event"], "peloton_auth_failed");
        assert_eq!(json["level"], "ERROR");
        assert!(json.get("fields").is_none());
    }

    #[test]
    fn panic_hook_logs_the_location_without_the_payload() {
        let buffer = Buffer::default();
        let subscriber = super::subscriber(buffer.clone(), EnvFilter::new("info"));
        super::install_panic_hook();
        let caught = tracing::subscriber::with_default(subscriber, || {
            std::panic::catch_unwind(|| panic!("secret-payload"))
        });
        // Back to the default hook (each test runs in its own process under
        // nextest; this keeps `cargo test` runs tidy too).
        drop(std::panic::take_hook());
        assert!(caught.is_err());
        let line = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(!line.contains("secret-payload"), "{line}");
        let json: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(json["event"], "panic");
        assert_eq!(json["level"], "ERROR");
        assert!(
            json["location"]
                .as_str()
                .is_some_and(|l| l.contains("logging.rs")),
            "{json}"
        );
    }
}
