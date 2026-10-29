//! End-to-end tests of the `healthcheck` subcommand: the real binary against
//! a server on the address it is configured to probe.

use std::net::SocketAddr;
use std::process::Output;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::routing::get;
use peloton_relay::config::LISTEN_VAR;
use time::macros::datetime;
use tokio::net::TcpListener;
use tokio::process::Command;
use wiremock::MockServer;

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod common;

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod api_common;

/// Serves `app` on a free loopback port.
async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr
}

/// Runs `peloton-relay healthcheck` configured to probe `listen`.
async fn healthcheck(listen: SocketAddr) -> Output {
    Command::new(env!("CARGO_BIN_EXE_peloton-relay"))
        .arg("healthcheck")
        .env_clear()
        .env(LISTEN_VAR, listen.to_string())
        .output()
        .await
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[tokio::test]
async fn healthcheck_succeeds_against_a_healthy_server() {
    let upstream = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let relay = api_common::relay(&dir, &upstream, true, datetime!(2030-01-01 00:00 UTC));
    let addr = serve(relay).await;
    let output = healthcheck(addr).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
}

#[tokio::test]
async fn healthcheck_fails_against_an_unhealthy_server() {
    let app = Router::new().route(
        "/healthz",
        get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
    );
    let output = healthcheck(serve(app).await).await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "error: unhealthy: status 503\n");
}

#[tokio::test]
async fn healthcheck_does_not_follow_a_redirect() {
    // A redirect to a healthy page must not count as healthy.
    let app = Router::new()
        .route(
            "/healthz",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/elsewhere")]) }),
        )
        .route("/elsewhere", get(|| async { StatusCode::OK }));
    let output = healthcheck(serve(app).await).await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "error: unhealthy: status 302\n");
}

#[tokio::test]
async fn healthcheck_fails_when_nothing_listens() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let output = healthcheck(addr).await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "error: the server is unreachable\n");
}
