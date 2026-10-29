//! A server that holds requests at a gate, for observing concurrency.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse as _, Response};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// A server that answers every request with a fixed JSON body once the test
/// lets it, and records how many requests it held at once.
///
/// Requests arrive, wait at a gate, and leave when [`Self::release`] opens it,
/// so a test observes concurrency without relying on timing.
pub struct GatedServer {
    addr: SocketAddr,
    arrived: Arc<Semaphore>,
    gate: Arc<Semaphore>,
    peak: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct GateState {
    body: Arc<str>,
    arrived: Arc<Semaphore>,
    gate: Arc<Semaphore>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

async fn gated(State(state): State<GateState>) -> Response {
    let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.peak.fetch_max(now, Ordering::SeqCst);
    state.arrived.add_permits(1);
    state.gate.acquire().await.unwrap().forget();
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    ([(CONTENT_TYPE, "application/json")], state.body.to_string()).into_response()
}

impl GatedServer {
    /// Starts a server answering `body` to every request.
    pub async fn start(body: String) -> Self {
        let arrived = Arc::new(Semaphore::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let state = GateState {
            body: body.into(),
            arrived: Arc::clone(&arrived),
            gate: Arc::clone(&gate),
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak: Arc::clone(&peak),
        };
        let app = Router::new().fallback(gated).with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            addr,
            arrived,
            gate,
            peak,
        }
    }

    /// The server's base URL.
    pub fn uri(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Waits until `count` more requests have reached the gate.
    ///
    /// # Panics
    ///
    /// When they have not all arrived after a generous wait, so a regression
    /// that holds requests back fails the test instead of hanging it.
    pub async fn wait_for_arrivals(&self, count: u32) {
        tokio::time::timeout(Duration::from_secs(30), self.arrived.acquire_many(count))
            .await
            .unwrap_or_else(|_| panic!("fewer than {count} requests reached the server"))
            .unwrap()
            .forget();
    }

    /// Whether no further request reaches the gate for a short while. Only a
    /// failing test waits for the whole while.
    pub async fn stays_quiet(&self) -> bool {
        tokio::time::timeout(Duration::from_millis(100), self.arrived.acquire())
            .await
            .is_err()
    }

    /// Lets every held and every future request through.
    pub fn release(&self) {
        self.gate.add_permits(Semaphore::MAX_PERMITS / 2);
    }

    /// The most requests the server held at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}
