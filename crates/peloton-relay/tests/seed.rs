//! Tests for `peloton-relay seed` and `peloton-relay bind`.

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod common;

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use peloton::body::MAX_RESPONSE_BODY_BYTES;
use peloton::ids::PelotonId;
use peloton_relay::account::{AccountName, Accounts};
use peloton_relay::seed::{SeedError, Seeded, run_bind, run_seed};
use peloton_relay::store::{StateFile, StoreError};
use secrecy::ExposeSecret as _;
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKENS: &str = r#"{"access_token":"access-secret","refresh_token":"refresh-secret","expires_at":"2030-01-01T00:00:00Z"}"#;
const NEXT_TOKENS: &str = r#"{"access_token":"next-access","refresh_token":"next-refresh","expires_at":"2031-01-01T00:00:00Z"}"#;

/// A Peloton user other than [`common::USER`].
const OTHER_USER: &str = "0123456789abcdef0123456789ab00a1";

fn other_user() -> PelotonId {
    OTHER_USER.parse().unwrap()
}

fn base(server: &MockServer) -> Url {
    server.uri().parse().unwrap()
}

fn alice() -> AccountName {
    common::account("alice")
}

fn bob() -> AccountName {
    common::account("bob")
}

/// The accounts `alice` and `bob` under `root`.
fn accounts(root: &Path) -> Accounts {
    Accounts::new(root.to_owned(), vec![alice(), bob()]).unwrap()
}

fn state_file(root: &Path, account: &AccountName) -> StateFile {
    accounts(root).state_file(account)
}

/// Binds `account` to `user` the way a writer does, without seeding it.
fn bind_directly(root: &Path, account: &AccountName, user: &PelotonId) {
    let accounts = accounts(root);
    accounts.ensure_dir(account).unwrap();
    accounts
        .state_file(account)
        .lock(common::LOCK_WAIT)
        .unwrap()
        .bind(user)
        .unwrap();
}

/// Writes a state file of `user` for `account`, leaving the binding alone.
fn write_state(root: &Path, account: &AccountName, user: &PelotonId) {
    let accounts = accounts(root);
    accounts.ensure_dir(account).unwrap();
    let mut state = common::state("stored-access", "stored-refresh", time::Duration::hours(48));
    state.user_id = user.clone();
    accounts
        .state_file(account)
        .save(&state, common::LOCK_WAIT)
        .unwrap();
}

fn bound_user(root: &Path, account: &AccountName) -> Option<PelotonId> {
    state_file(root, account)
        .read_binding()
        .unwrap()
        .map(|binding| binding.user_id)
}

/// The content of every state and binding file under `root`, to show a
/// refused command wrote nothing.
fn files(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    [alice(), bob()]
        .iter()
        .flat_map(|account| {
            let file = state_file(root, account);
            [file.path.clone(), file.binding_path()]
        })
        .map(|path| {
            let content = fs::read(&path).ok();
            (path, content)
        })
        .collect()
}

fn me_mock(user: &str) -> Mock {
    Mock::given(method("GET")).and(path("/me")).respond_with(
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": user,
            "username": "rider"
        })),
    )
}

async fn me_server(user: &str) -> MockServer {
    let server = MockServer::start().await;
    me_mock(user).mount(&server).await;
    server
}

async fn seed_with(
    input: &str,
    root: &Path,
    server: &MockServer,
    account: &AccountName,
    rebind: bool,
) -> Result<Seeded, SeedError> {
    run_seed(
        input.as_bytes(),
        &accounts(root),
        account,
        rebind,
        &common::http(),
        &base(server),
        common::LOCK_WAIT,
    )
    .await
}

async fn seed(
    root: &Path,
    server: &MockServer,
    account: &AccountName,
    rebind: bool,
) -> Result<Seeded, SeedError> {
    seed_with(TOKENS, root, server, account, rebind).await
}

fn is_account(err: &SeedError) -> Option<&AccountName> {
    match err {
        SeedError::UnknownAccount { account }
        | SeedError::BoundToAnotherUser { account }
        | SeedError::UserBoundElsewhere { account }
        | SeedError::BindingUnreadable { account, .. }
        | SeedError::NoState { account } => Some(account),
        _ => None,
    }
}

#[tokio::test]
async fn seed_writes_state_with_user_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer access-secret"))
        .and(header("peloton-platform", "web"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": common::USER,
            "username": "rider"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let seeded = seed(dir.path(), &server, &alice(), false).await.unwrap();

    assert_eq!(seeded.account, alice());
    assert_eq!(seeded.username.as_deref(), Some("rider"));
    assert_eq!(seeded.user_id, common::user());
    assert_eq!(seeded.previous, None);
    let state = state_file(dir.path(), &alice()).load().unwrap().unwrap();
    assert_eq!(state.user_id, common::user());
    assert_eq!(state.tokens.access_token.expose_secret(), "access-secret");
    assert_eq!(state.tokens.refresh_token.expose_secret(), "refresh-secret");
    assert!(!state_file(dir.path(), &bob()).path.exists());
}

#[tokio::test]
async fn first_seed_binds_the_account() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../peloton/tests/fixtures/me.json");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(fs::read(fixture).unwrap(), "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let seeded = seed(dir.path(), &server, &alice(), false).await.unwrap();

    assert_eq!(bound_user(dir.path(), &alice()), Some(common::user()));
    assert_eq!(
        seeded.to_string(),
        "account \"alice\": bound to Peloton user username-1 (id \u{2026}00f2)"
    );
}

#[tokio::test]
async fn seed_without_a_username_says_so() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": common::USER })),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let seeded = seed(dir.path(), &server, &alice(), false).await.unwrap();

    assert_eq!(seeded.username, None);
    assert_eq!(
        seeded.to_string(),
        "account \"alice\": bound to Peloton user (no username) (id \u{2026}00f2)"
    );
}

#[tokio::test]
async fn seed_output_never_carries_control_characters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": common::USER,
            "username": "rider\u{1b}[2J\nforged"
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let shown = seed(dir.path(), &server, &alice(), false)
        .await
        .unwrap()
        .to_string();

    assert!(!shown.chars().any(char::is_control), "{shown:?}");
    assert_eq!(
        shown,
        "account \"alice\": bound to Peloton user rider\u{fffd}[2J\u{fffd}forged (id \u{2026}00f2)"
    );
}

#[tokio::test]
async fn reseed_of_the_same_user_replaces_the_tokens() {
    let server = MockServer::start().await;
    me_mock(common::USER).expect(2).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &server, &alice(), false).await.unwrap();
    let binding = state_file(dir.path(), &alice()).binding_path();
    let bound = fs::metadata(&binding).unwrap();

    let seeded = seed_with(NEXT_TOKENS, dir.path(), &server, &alice(), false)
        .await
        .unwrap();

    assert_eq!(seeded.previous, None);
    assert_eq!(bound_user(dir.path(), &alice()), Some(common::user()));
    assert_eq!(
        fs::metadata(&binding).unwrap().ino(),
        bound.ino(),
        "an unchanged binding was rewritten"
    );
    let state = state_file(dir.path(), &alice()).load().unwrap().unwrap();
    assert_eq!(state.tokens.access_token.expose_secret(), "next-access");
    assert_eq!(state.tokens.refresh_token.expose_secret(), "next-refresh");
}

#[tokio::test]
async fn seed_of_another_user_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &alice(), &other_user());
    bind_directly(dir.path(), &alice(), &other_user());
    let before = files(dir.path());
    let server = me_server(common::USER).await;

    let err = seed(dir.path(), &server, &alice(), false)
        .await
        .unwrap_err();

    assert!(
        matches!(err, SeedError::BoundToAnotherUser { .. }),
        "{err:?}"
    );
    assert_eq!(is_account(&err), Some(&alice()));
    assert_eq!(
        err.to_string(),
        "account \"alice\" is bound to another Peloton user; seed with --rebind to replace it"
    );
    assert_eq!(files(dir.path()), before);
}

#[tokio::test]
async fn rebind_replaces_the_binding() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &alice(), &other_user());
    bind_directly(dir.path(), &alice(), &other_user());
    let server = me_server(common::USER).await;

    let seeded = seed(dir.path(), &server, &alice(), true).await.unwrap();

    assert_eq!(seeded.previous, Some(other_user()));
    assert_eq!(
        seeded.to_string(),
        "account \"alice\": was bound to Peloton user id \u{2026}00a1\n\
         account \"alice\": bound to Peloton user rider (id \u{2026}00f2)"
    );
    assert_eq!(bound_user(dir.path(), &alice()), Some(common::user()));
    let state = state_file(dir.path(), &alice()).load().unwrap().unwrap();
    assert_eq!(state.user_id, common::user());
    assert_eq!(state.tokens.access_token.expose_secret(), "access-secret");
}

#[tokio::test]
async fn rebind_of_the_same_user_reports_no_previous_user() {
    let dir = tempfile::tempdir().unwrap();
    bind_directly(dir.path(), &alice(), &common::user());
    let server = me_server(common::USER).await;

    let seeded = seed(dir.path(), &server, &alice(), true).await.unwrap();

    assert_eq!(seeded.previous, None);
    assert!(!seeded.to_string().contains("was bound"));
}

#[tokio::test]
async fn seed_refuses_a_user_bound_to_another_account() {
    for rebind in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        bind_directly(dir.path(), &bob(), &common::user());
        let before = files(dir.path());
        let server = me_server(common::USER).await;

        let err = seed(dir.path(), &server, &alice(), rebind)
            .await
            .unwrap_err();

        assert!(
            matches!(err, SeedError::UserBoundElsewhere { .. }),
            "{err:?}"
        );
        assert_eq!(is_account(&err), Some(&bob()));
        assert_eq!(
            err.to_string(),
            "this Peloton user is already bound to account \"bob\""
        );
        assert!(!state_file(dir.path(), &alice()).path.exists());
        assert!(!state_file(dir.path(), &alice()).binding_path().exists());
        assert_eq!(files(dir.path()), before);
    }
}

#[tokio::test]
async fn seed_refuses_when_another_binding_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path()).ensure_dir(&bob()).unwrap();
    fs::write(state_file(dir.path(), &bob()).binding_path(), "{").unwrap();
    let before = files(dir.path());
    let server = me_server(common::USER).await;

    let err = seed(dir.path(), &server, &alice(), false)
        .await
        .unwrap_err();

    assert!(
        matches!(err, SeedError::BindingUnreadable { .. }),
        "{err:?}"
    );
    assert_eq!(is_account(&err), Some(&bob()));
    assert_eq!(
        err.to_string(),
        "cannot read the binding of account \"bob\""
    );
    assert_eq!(files(dir.path()), before);
}

#[tokio::test]
async fn seed_refuses_when_its_own_binding_is_unreadable() {
    for rebind in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        accounts(dir.path()).ensure_dir(&alice()).unwrap();
        fs::write(state_file(dir.path(), &alice()).binding_path(), "{").unwrap();
        let before = files(dir.path());
        let server = me_server(common::USER).await;

        let err = seed(dir.path(), &server, &alice(), rebind)
            .await
            .unwrap_err();

        assert!(
            matches!(err, SeedError::BindingUnreadable { .. }),
            "{err:?}"
        );
        assert_eq!(is_account(&err), Some(&alice()));
        assert_eq!(files(dir.path()), before);
    }
}

/// The store error behind an unreadable-binding refusal.
fn binding_cause(err: &SeedError) -> &StoreError {
    assert!(
        matches!(err, SeedError::BindingUnreadable { .. }),
        "{err:?}"
    );
    std::error::Error::source(err)
        .and_then(|source| source.downcast_ref::<StoreError>())
        .unwrap_or_else(|| panic!("no store error behind {err:?}"))
}

#[tokio::test]
async fn unreadable_binding_refusal_keeps_its_cause() {
    let server = me_server(common::USER).await;

    // Another account's binding does not parse.
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path()).ensure_dir(&bob()).unwrap();
    fs::write(state_file(dir.path(), &bob()).binding_path(), "{").unwrap();
    let err = seed(dir.path(), &server, &alice(), false)
        .await
        .unwrap_err();
    assert_eq!(is_account(&err), Some(&bob()));
    assert!(
        matches!(binding_cause(&err), StoreError::Corrupt),
        "{err:?}"
    );

    // The account's own binding does not parse.
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path()).ensure_dir(&alice()).unwrap();
    fs::write(state_file(dir.path(), &alice()).binding_path(), "{").unwrap();
    let err = seed(dir.path(), &server, &alice(), true).await.unwrap_err();
    assert_eq!(is_account(&err), Some(&alice()));
    assert!(
        matches!(binding_cause(&err), StoreError::Corrupt),
        "{err:?}"
    );

    // Another account's directory cannot be searched.
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path()).ensure_dir(&bob()).unwrap();
    let bob_dir = dir.path().join("bob");
    fs::set_permissions(&bob_dir, fs::Permissions::from_mode(0o000)).unwrap();
    let enforced = fs::read_dir(&bob_dir).is_err();
    let result = seed(dir.path(), &server, &alice(), false).await;
    fs::set_permissions(&bob_dir, fs::Permissions::from_mode(0o700)).unwrap();
    if !enforced {
        eprintln!("skipped the permission case: directory permissions are not enforced (root)");
        return;
    }
    let err = result.unwrap_err();
    assert_eq!(is_account(&err), Some(&bob()));
    assert!(
        matches!(
            binding_cause(&err),
            StoreError::Io(cause) if cause.kind() == io::ErrorKind::PermissionDenied
        ),
        "{err:?}"
    );
    assert!(!state_file(dir.path(), &alice()).path.exists());
}

/// An input that fails the test when read.
struct PanicsOnRead;

impl Read for PanicsOnRead {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("the input of an unconfigured account was read");
    }
}

#[tokio::test]
async fn seed_refuses_an_unconfigured_account() {
    let server = MockServer::start().await;
    me_mock(common::USER).expect(0).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let carol = common::account("carol");

    let err = run_seed(
        PanicsOnRead,
        &accounts(dir.path()),
        &carol,
        false,
        &common::http(),
        &base(&server),
        common::LOCK_WAIT,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::UnknownAccount { .. }), "{err:?}");
    assert_eq!(is_account(&err), Some(&carol));
    assert_eq!(
        err.to_string(),
        "account \"carol\" is not configured (PELOTON_RELAY_ACCOUNTS)"
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn seed_holds_the_bindings_lock() {
    let dir = tempfile::tempdir().unwrap();
    let server = me_server(common::USER).await;
    let _binding = accounts(dir.path())
        .lock_bindings(common::LOCK_WAIT)
        .unwrap();

    let err = run_seed(
        TOKENS.as_bytes(),
        &accounts(dir.path()),
        &alice(),
        false,
        &common::http(),
        &base(&server),
        Duration::from_millis(200),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::Busy), "{err:?}");
    assert!(!state_file(dir.path(), &alice()).path.exists());
    assert!(!state_file(dir.path(), &alice()).binding_path().exists());
}

#[tokio::test]
async fn seed_rejects_invalid_json() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let err = seed_with(
        "{\"access_token\": \"leaky-secret\"",
        dir.path(),
        &server,
        &alice(),
        false,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::InvalidInput));
    assert!(!format!("{err} {err:?}").contains("leaky-secret"));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!state_file(dir.path(), &alice()).path.exists());
}

#[tokio::test]
async fn seed_rejects_oversized_input() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let input = " ".repeat(128 * 1024);

    let err = seed_with(&input, dir.path(), &server, &alice(), false)
        .await
        .unwrap_err();

    assert!(matches!(err, SeedError::InvalidInput));
    assert!(!state_file(dir.path(), &alice()).path.exists());
}

#[tokio::test]
async fn seed_rejects_token_peloton_refuses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(401).set_body_string("body-secret"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let err = seed(dir.path(), &server, &alice(), false)
        .await
        .unwrap_err();

    assert!(matches!(err, SeedError::Rejected { status: 401 }));
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains("body-secret") && !shown.contains("access-secret"));
    assert!(!state_file(dir.path(), &alice()).path.exists());
    assert!(!state_file(dir.path(), &alice()).binding_path().exists());
}

#[tokio::test]
async fn seed_reports_other_statuses_neutrally() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let err = seed(dir.path(), &server, &alice(), false)
        .await
        .unwrap_err();

    assert!(matches!(err, SeedError::Unexpected { status: 503 }));
    assert_eq!(err.to_string(), "Peloton answered with status 503");
    assert!(!state_file(dir.path(), &alice()).path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seed_waits_for_the_state_lock_holder() {
    let server = MockServer::start().await;
    me_mock(common::USER).expect(1).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path()).ensure_dir(&alice()).unwrap();
    let store = state_file(dir.path(), &alice());
    // The relay, mid-refresh, holds the lock through its own handle.
    let refreshing = store.lock(common::LOCK_WAIT).unwrap();

    let seeding = tokio::spawn({
        let root = dir.path().to_owned();
        let base = base(&server);
        async move {
            run_seed(
                TOKENS.as_bytes(),
                &accounts(&root),
                &alice(),
                false,
                &common::http(),
                &base,
                common::LOCK_WAIT,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!seeding.is_finished(), "seed did not wait for the lock");
    assert!(!store.path.exists());
    assert!(!store.binding_path().exists());

    drop(refreshing);
    let seeded = seeding.await.unwrap().unwrap();
    assert_eq!(seeded.username.as_deref(), Some("rider"));
    let state = store.load().unwrap().unwrap();
    assert_eq!(state.tokens.refresh_token.expose_secret(), "refresh-secret");
}

#[tokio::test]
async fn seed_gives_up_clearly_when_the_lock_stays_held() {
    let server = me_server(common::USER).await;
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path()).ensure_dir(&alice()).unwrap();
    let store = state_file(dir.path(), &alice());
    let _refreshing = store.lock(common::LOCK_WAIT).unwrap();

    let err = run_seed(
        TOKENS.as_bytes(),
        &accounts(dir.path()),
        &alice(),
        false,
        &common::http(),
        &base(&server),
        Duration::from_millis(100),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SeedError::Busy), "{err:?}");
    assert_eq!(
        err.to_string(),
        "another seed, bind or token refresh holds the lock; try again"
    );
    assert!(!store.path.exists());
}

/// A `/me` answer padded with trailing whitespace to `len` bytes.
fn me_of_len(len: usize) -> String {
    let me = format!(r#"{{"id":"{}","username":"rider"}}"#, common::USER);
    format!("{me}{}", " ".repeat(len - me.len()))
}

async fn me_answering(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn seed_reads_a_profile_at_the_size_limit() {
    let server = me_answering(me_of_len(MAX_RESPONSE_BODY_BYTES)).await;
    let dir = tempfile::tempdir().unwrap();

    let seeded = seed(dir.path(), &server, &alice(), false).await.unwrap();

    assert_eq!(seeded.username.as_deref(), Some("rider"));
}

#[tokio::test]
async fn seed_rejects_a_profile_over_the_size_limit() {
    let server = me_answering(me_of_len(MAX_RESPONSE_BODY_BYTES + 1)).await;
    let dir = tempfile::tempdir().unwrap();

    let err = seed(dir.path(), &server, &alice(), false)
        .await
        .unwrap_err();

    assert!(matches!(err, SeedError::Upstream));
    assert!(!state_file(dir.path(), &alice()).path.exists());
}

#[test]
fn bind_records_the_state_files_user() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &alice(), &common::user());
    let state = fs::read(state_file(dir.path(), &alice()).path).unwrap();

    let bound = run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap();

    assert_eq!(bound.account, alice());
    assert_eq!(bound.user_id, common::user());
    assert_eq!(
        bound.to_string(),
        "account \"alice\": bound to the Peloton user of its state file (id \u{2026}00f2)"
    );
    assert_eq!(bound_user(dir.path(), &alice()), Some(common::user()));
    assert_eq!(
        fs::read(state_file(dir.path(), &alice()).path).unwrap(),
        state,
        "bind changed the state file"
    );
}

#[test]
fn bind_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &alice(), &common::user());
    run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap();
    let binding = state_file(dir.path(), &alice()).binding_path();
    let first = fs::metadata(&binding).unwrap();

    let bound = run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap();

    assert_eq!(bound.user_id, common::user());
    assert_eq!(
        fs::metadata(&binding).unwrap().ino(),
        first.ino(),
        "an unchanged binding was rewritten"
    );
}

#[test]
fn bind_refuses_another_user() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &alice(), &common::user());
    bind_directly(dir.path(), &alice(), &other_user());
    let before = files(dir.path());

    let err = run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap_err();

    assert!(
        matches!(err, SeedError::BoundToAnotherUser { .. }),
        "{err:?}"
    );
    assert_eq!(is_account(&err), Some(&alice()));
    assert_eq!(files(dir.path()), before);
}

#[test]
fn bind_refuses_a_user_bound_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &alice(), &common::user());
    bind_directly(dir.path(), &bob(), &common::user());
    let before = files(dir.path());

    let err = run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap_err();

    assert!(
        matches!(err, SeedError::UserBoundElsewhere { .. }),
        "{err:?}"
    );
    assert_eq!(is_account(&err), Some(&bob()));
    assert!(!state_file(dir.path(), &alice()).binding_path().exists());
    assert_eq!(files(dir.path()), before);
}

#[test]
fn bind_refuses_when_another_binding_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &alice(), &common::user());
    accounts(dir.path()).ensure_dir(&bob()).unwrap();
    fs::write(state_file(dir.path(), &bob()).binding_path(), "{").unwrap();
    let before = files(dir.path());

    let err = run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap_err();

    assert!(
        matches!(binding_cause(&err), StoreError::Corrupt),
        "{err:?}"
    );
    assert_eq!(is_account(&err), Some(&bob()));
    assert_eq!(files(dir.path()), before);
}

#[test]
fn bind_without_state_is_refused() {
    let dir = tempfile::tempdir().unwrap();

    // Neither the account directory nor its state file exists.
    let err = run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap_err();
    assert!(matches!(err, SeedError::NoState { .. }), "{err:?}");
    assert_eq!(is_account(&err), Some(&alice()));
    assert_eq!(
        err.to_string(),
        "account \"alice\" has no state file to bind"
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

    // The directory exists, the state file does not.
    accounts(dir.path()).ensure_dir(&alice()).unwrap();
    let err = run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap_err();
    assert!(matches!(err, SeedError::NoState { .. }), "{err:?}");
    assert!(!state_file(dir.path(), &alice()).binding_path().exists());
}

#[test]
fn bind_refuses_a_corrupt_state_file() {
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path()).ensure_dir(&alice()).unwrap();
    fs::write(state_file(dir.path(), &alice()).path, "{").unwrap();

    let err = run_bind(&accounts(dir.path()), &alice(), common::LOCK_WAIT).unwrap_err();

    assert!(
        matches!(err, SeedError::Store(StoreError::Corrupt)),
        "{err:?}"
    );
    assert!(!state_file(dir.path(), &alice()).binding_path().exists());
}

#[test]
fn bind_refuses_an_unconfigured_account() {
    let dir = tempfile::tempdir().unwrap();
    let carol = common::account("carol");

    let err = run_bind(&accounts(dir.path()), &carol, common::LOCK_WAIT).unwrap_err();

    assert!(matches!(err, SeedError::UnknownAccount { .. }), "{err:?}");
    assert_eq!(is_account(&err), Some(&carol));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn bind_holds_the_bindings_lock() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &alice(), &common::user());
    let _binding = accounts(dir.path())
        .lock_bindings(common::LOCK_WAIT)
        .unwrap();

    let err = run_bind(&accounts(dir.path()), &alice(), Duration::from_millis(200)).unwrap_err();

    assert!(matches!(err, SeedError::Busy), "{err:?}");
    assert!(!state_file(dir.path(), &alice()).binding_path().exists());
}
