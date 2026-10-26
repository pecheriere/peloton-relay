//! Helpers shared by the integration tests.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration as StdDuration;

use peloton::ids::PelotonId;
use peloton::oauth::TokenSet;
use peloton_relay::store::{State, StateFile};
use secrecy::SecretString;
use time::{Duration, OffsetDateTime};
use tracing_subscriber::fmt::MakeWriter;

/// The credential owner: the member the scrubbed fixtures belong to.
pub const USER: &str = "000000000000000000000000000000f2";

pub fn user() -> PelotonId {
    USER.parse().unwrap()
}

pub fn state(access: &str, refresh: &str, expires_in: Duration) -> State {
    State {
        tokens: TokenSet {
            access_token: SecretString::from(access.to_owned()),
            refresh_token: SecretString::from(refresh.to_owned()),
            expires_at: OffsetDateTime::now_utc() + expires_in,
        },
        user_id: user(),
    }
}

pub fn state_file(dir: &Path) -> StateFile {
    StateFile {
        path: dir.join("state.json"),
    }
}

/// An HTTP client configured as production must configure it: no redirects,
/// bounded timeouts.
pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(StdDuration::from_secs(5))
        .timeout(StdDuration::from_secs(10))
        .build()
        .unwrap()
}

/// Collects formatted tracing output in memory.
#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    pub fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Process-wide log capture.
///
/// A global subscriber rather than a thread-local one: tracing caches callsite
/// interest across threads, so a scoped subscriber misses events when other
/// tests hit the same callsites concurrently. Tests tag their events with a
/// span and filter on it.
pub fn captured_logs() -> &'static LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt().with_writer(logs.clone()).finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        logs
    })
}
