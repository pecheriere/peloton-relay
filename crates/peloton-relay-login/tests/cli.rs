//! The helper binary's handling of the relay options, before any sign-in.

use std::process::{Command, Stdio};

const CODE: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn posting_without_a_terminal_needs_yes() {
    let output = Command::new(env!("CARGO_BIN_EXE_peloton-relay-login"))
        .args([
            "--account",
            "bob",
            "--post-to",
            "https://relay.example.net/admin/seed",
            "--code",
            CODE,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("pass --yes"), "{stderr}");
    assert!(!stderr.contains(CODE), "{stderr}");
    assert_eq!(output.stdout, b"");
}
