//! The per-account binding file and the cross-account bindings lock.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use peloton::ids::PelotonId;
use peloton_relay::account::{AccountName, Accounts};
use peloton_relay::store::{Binding, StateFile, StoreError};

const USER: &str = "0123456789abcdef0123456789abcd0f";
const WAIT: Duration = Duration::from_secs(5);

fn user() -> PelotonId {
    USER.parse().unwrap()
}

fn setup() -> (tempfile::TempDir, Accounts, StateFile) {
    let root = tempfile::tempdir().unwrap();
    let alice: AccountName = "alice".parse().unwrap();
    let accounts = Accounts::new(root.path().to_path_buf(), vec![alice.clone()]).unwrap();
    accounts.ensure_dir(&alice).unwrap();
    let file = accounts.state_file(&alice);
    (root, accounts, file)
}

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn binding_round_trips() {
    let (_root, _accounts, file) = setup();
    assert_eq!(file.read_binding().unwrap(), None);

    let lock = file.lock(WAIT).unwrap();
    assert_eq!(lock.binding().unwrap(), None);
    lock.bind(&user()).unwrap();

    let expected = Some(Binding { user_id: user() });
    assert_eq!(file.read_binding().unwrap(), expected);
    assert_eq!(lock.binding().unwrap(), expected);
    assert_eq!(file.binding_path().file_name().unwrap(), "binding.json");
}

#[test]
fn corrupt_binding_is_reported_without_content() {
    let (_root, _accounts, file) = setup();
    let lock = file.lock(WAIT).unwrap();
    let extra = format!(r#"{{"user_id":"{USER}","secretkey":1}}"#);
    for text in [r#"{"user_id":"#, extra.as_str(), ""] {
        fs::write(file.binding_path(), text).unwrap();
        for result in [file.read_binding(), lock.binding()] {
            let err = result.unwrap_err();
            assert!(matches!(err, StoreError::Corrupt), "{err:?}");
            for shown in [err.to_string(), format!("{err:?}")] {
                assert!(!shown.contains("user_id"), "{shown}");
                assert!(!shown.contains("secretkey"), "{shown}");
                assert!(!shown.contains(USER), "{shown}");
            }
        }
    }
}

#[test]
fn binding_is_written_private_and_atomically() {
    let (_root, _accounts, file) = setup();
    let path = file.binding_path();
    fs::write(&path, "{}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    file.lock(WAIT).unwrap().bind(&user()).unwrap();

    assert_eq!(mode(&path), 0o600);
    let mut names: Vec<String> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    // Only the binding and the lock file: no temp file is left behind.
    assert_eq!(names, [".state.json.lock", "binding.json"]);
    assert_eq!(file.read_binding().unwrap().unwrap().user_id, user());
}

#[test]
fn bindings_lock_excludes_a_second_holder() {
    let (_root, accounts, _file) = setup();
    let held = accounts.lock_bindings(WAIT).unwrap();

    let second = accounts.lock_bindings(Duration::from_millis(100));
    assert!(matches!(second, Err(StoreError::Locked)));
    assert_eq!(mode(&accounts.root().join(".bindings.lock")), 0o600);

    drop(held);
    accounts.lock_bindings(WAIT).unwrap();
}

#[test]
fn binding_file_holds_exactly_the_user_id() {
    let (_root, _accounts, file) = setup();
    file.lock(WAIT).unwrap().bind(&user()).unwrap();
    assert_eq!(
        fs::read(file.binding_path()).unwrap(),
        format!(r#"{{"user_id":"{USER}"}}"#).into_bytes()
    );
}

#[test]
fn store_errors_name_the_files_they_cover() {
    assert_eq!(
        StoreError::Corrupt.to_string(),
        "state or binding file is empty or corrupt"
    );
    assert_eq!(
        StoreError::Io(std::io::Error::other("cause")).to_string(),
        "state, binding or lock file I/O failed"
    );
    assert_eq!(
        StoreError::Locked.to_string(),
        "state or bindings lock file is held by another writer"
    );
}
