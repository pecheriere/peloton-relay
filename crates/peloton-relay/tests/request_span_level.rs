//! The request span's fields reach warnings even when `RUST_LOG=warn`.
//!
//! In a binary of its own: the subscriber is global, with its own filter.

use time::macros::datetime;
use tracing_subscriber::EnvFilter;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

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

#[tokio::test]
async fn warning_carries_the_request_span_under_a_warn_filter() {
    let logs = common::LogBuffer::default();
    tracing::subscriber::set_global_default(peloton_relay::logging::subscriber(
        logs.clone(),
        EnvFilter::new("warn"),
    ))
    .unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = api_common::relay(&dir, &server, true, datetime!(2030-01-01 00:00 UTC));
    let (_, headers, _) = api_common::send(&app, api_common::bare("GET", "/v1/me")).await;
    let id = headers["x-request-id"].to_str().unwrap();

    let contents = logs.contents();
    let warning: serde_json::Value = contents
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|line| line["message"] == "upstream request failed")
        .unwrap_or_else(|| panic!("no upstream warning: {contents}"));
    assert_eq!(warning["span"]["route"], "/v1/me");
    assert_eq!(warning["span"]["request_id"], id);
}
