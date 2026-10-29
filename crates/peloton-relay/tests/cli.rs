//! End-to-end tests of the `seed` and `bind` command lines: the real binary,
//! without network access.

use std::path::Path;
use std::process::{Command, Output};

use peloton_relay::config::{ACCOUNTS_VAR, LISTEN_VAR, STATE_DIR_VAR};

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

/// A running `peloton-relay serve` on a free loopback port.
struct Server {
    child: std::process::Child,
    port: u16,
}

impl Server {
    fn start(state_dir: &Path, accounts: &str) -> Self {
        use std::process::Stdio;

        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(env!("CARGO_BIN_EXE_peloton-relay"))
            .arg("serve")
            .env_clear()
            .env(ACCOUNTS_VAR, accounts)
            .env(STATE_DIR_VAR, state_dir)
            .env(LISTEN_VAR, format!("127.0.0.1:{port}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self { child, port }
    }

    /// The status of `GET path`, or `None` while the server is unreachable.
    fn status(&self, path: &str) -> Option<u16> {
        use std::io::{Read as _, Write as _};

        let mut stream = std::net::TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok()?;
        write!(
            stream,
            "GET {path} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        response.split(' ').nth(1)?.parse().ok()
    }

    fn wait_until_serving(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self.status("/healthz") != Some(200) {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "the server exited early"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "the server did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Sends SIGTERM and returns the output once the server has exited.
    fn terminate(self) -> Output {
        let signalled = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(signalled.success());
        self.child.wait_with_output().unwrap()
    }
}

#[test]
fn serve_refuses_to_start_without_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_peloton-relay"))
        .arg("serve")
        .env_clear()
        .env(STATE_DIR_VAR, dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("PELOTON_RELAY_ACCOUNTS must list"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn serve_refuses_a_single_account_state_file() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("state.json");
    std::fs::write(&legacy, "legacy").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_peloton-relay"))
        .arg("serve")
        .env_clear()
        .env(ACCOUNTS_VAR, "alice,bob")
        .env(STATE_DIR_VAR, dir.path())
        .env(LISTEN_VAR, "127.0.0.1:0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    let Ok(output) = receiver.recv_timeout(std::time::Duration::from_secs(5)) else {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        panic!("the server did not refuse to start");
    };
    let output = output.unwrap();
    assert_eq!(output.status.code(), Some(1));
    let shown = stderr(&output);
    assert!(shown.contains("move it to"), "{shown}");
    assert!(shown.contains("state.json"), "{shown}");
    assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "legacy");
}

#[test]
fn serve_creates_private_account_directories() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::start(dir.path(), "alice,bob");
    server.wait_until_serving();
    for name in ["alice", "bob"] {
        let mode = std::fs::metadata(dir.path().join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{name}");
    }
    assert_eq!(server.status("/readyz"), Some(200));
    assert_eq!(server.status("/v1/me"), Some(403));
    let output = server.terminate();
    assert!(output.status.success(), "{}", stderr(&output));
    let logged = String::from_utf8(output.stdout).unwrap();
    assert!(logged.contains(r#""accounts":"alice,bob""#), "{logged}");
}

#[test]
fn serve_runs_with_one_unusable_account_dir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bob"), "not a directory").unwrap();
    let mut server = Server::start(dir.path(), "alice,bob");
    server.wait_until_serving();
    let output = server.terminate();
    assert!(output.status.success(), "{}", stderr(&output));
    let logged = String::from_utf8(output.stdout).unwrap();
    assert!(
        logged
            .lines()
            .any(|line| line.contains(r#""level":"ERROR""#) && line.contains(r#""account":"bob""#)),
        "{logged}"
    );
    assert!(dir.path().join("alice").is_dir());
}
