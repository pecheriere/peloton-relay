//! End-to-end tests of the `seed` and `bind` command lines: the real binary,
//! without network access.

use std::path::Path;
use std::process::{Command, Output};

use peloton_relay::config::{ACCOUNTS_VAR, STATE_DIR_VAR};

/// Runs `peloton-relay <args>` serving `alice,bob` from `state_dir`.
fn relay(state_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_peloton-relay"))
        .args(args)
        .env_clear()
        .env(ACCOUNTS_VAR, "alice,bob")
        .env(STATE_DIR_VAR, state_dir)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().next().is_none()
}

#[test]
fn seed_requires_an_account() {
    let dir = tempfile::tempdir().unwrap();
    let output = relay(dir.path(), &["seed"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--account"), "{}", stderr(&output));
    assert!(is_empty(dir.path()));
}

#[test]
fn seed_refuses_an_unconfigured_account_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let output = relay(dir.path(), &["seed", "--account", "carol"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "error: account \"carol\" is not configured (PELOTON_RELAY_ACCOUNTS)\n"
    );
    assert!(is_empty(dir.path()));
}

#[test]
fn seed_rejects_an_invalid_account_name() {
    let dir = tempfile::tempdir().unwrap();
    let output = relay(dir.path(), &["seed", "--account", "Alice"]);
    assert!(!output.status.success());
    let shown = stderr(&output);
    assert!(shown.contains("invalid value 'Alice'"), "{shown}");
    assert!(
        shown.contains("an account name is 1 to 16 lowercase ASCII letters"),
        "{shown}"
    );
    assert!(is_empty(dir.path()));
}

#[test]
fn bind_requires_an_account() {
    let dir = tempfile::tempdir().unwrap();
    let output = relay(dir.path(), &["bind"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--account"), "{}", stderr(&output));
}

#[test]
fn bind_refuses_an_unconfigured_account_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let output = relay(dir.path(), &["bind", "--account", "carol"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "error: account \"carol\" is not configured (PELOTON_RELAY_ACCOUNTS)\n"
    );
    assert!(is_empty(dir.path()));
}

#[test]
fn bind_binds_the_state_files_user() {
    let dir = tempfile::tempdir().unwrap();
    let account = dir.path().join("alice");
    std::fs::create_dir(&account).unwrap();
    std::fs::write(
        account.join("state.json"),
        r#"{"tokens":{"access_token":"a","refresh_token":"r","expires_at":"2030-01-01T00:00:00Z"},"user_id":"000000000000000000000000000000f2"}"#,
    )
    .unwrap();

    let output = relay(dir.path(), &["bind", "--account", "alice"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stderr(&output),
        "account \"alice\": bound to the Peloton user of its state file (id \u{2026}00f2)\n"
    );
    assert_eq!(
        std::fs::read_to_string(account.join("binding.json")).unwrap(),
        r#"{"user_id":"000000000000000000000000000000f2"}"#
    );
}
