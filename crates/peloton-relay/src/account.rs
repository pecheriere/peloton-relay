//! Account names, the configured account list and the per-account state layout.

use std::fmt;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use peloton::ids::PelotonId;

use crate::store::{BindingsLock, StateFile, StoreError};

const MAX_NAME_LEN: usize = 16;
const STATE_FILE_NAME: &str = "state.json";
const BINDINGS_LOCK_NAME: &str = ".bindings.lock";
const ACCOUNT_DIR_MODE: u32 = 0o700;
const SUFFIX_LEN: usize = 4;

/// The name of an account: 1 to 16 lowercase ASCII letters, chosen by the
/// deployment. Being a bare path component, it is safe to join onto a path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountName(String);

/// The text is not a valid [`AccountName`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("an account name is 1 to 16 lowercase ASCII letters")]
pub struct InvalidAccountName;

impl FromStr for AccountName {
    type Err = InvalidAccountName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if (1..=MAX_NAME_LEN).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase()) {
            Ok(Self(s.to_owned()))
        } else {
            Err(InvalidAccountName)
        }
    }
}

impl fmt::Display for AccountName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AccountName {
    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The account list is unusable.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AccountsError {
    /// No account is configured.
    #[error("no account is configured")]
    Empty,
    /// An account is listed more than once.
    #[error("an account is listed more than once")]
    Duplicate,
}

/// The accounts one relay serves, and where their state lives:
/// `<root>/<account>/`.
#[derive(Debug, Clone)]
pub struct Accounts {
    root: PathBuf,
    names: Vec<AccountName>,
}

impl Accounts {
    /// The accounts `names`, in configuration order, under the state
    /// directory `root`.
    ///
    /// # Errors
    ///
    /// [`AccountsError`] when `names` is empty or has a repeated name.
    pub fn new(root: PathBuf, names: Vec<AccountName>) -> Result<Self, AccountsError> {
        if names.is_empty() {
            return Err(AccountsError::Empty);
        }
        let distinct = names
            .iter()
            .enumerate()
            .all(|(index, name)| !names[..index].contains(name));
        if !distinct {
            return Err(AccountsError::Duplicate);
        }
        Ok(Self { root, names })
    }

    /// The configured names, in configuration order.
    #[must_use]
    pub fn names(&self) -> &[AccountName] {
        &self.names
    }

    /// Whether `name` is a configured account.
    #[must_use]
    pub fn contains(&self, name: &AccountName) -> bool {
        self.names.contains(name)
    }

    /// The state directory holding every account directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The state file of `name`.
    #[must_use]
    pub fn state_file(&self, name: &AccountName) -> StateFile {
        StateFile {
            path: self.root.join(name.as_str()).join(STATE_FILE_NAME),
        }
    }

    /// The names and state files of every configured account but `name`.
    #[must_use]
    pub fn peers_of(&self, name: &AccountName) -> Vec<(AccountName, StateFile)> {
        self.names
            .iter()
            .filter(|other| *other != name)
            .map(|other| (other.clone(), self.state_file(other)))
            .collect()
    }

    /// Takes the cross-account lock `<root>/.bindings.lock`, waiting at most
    /// `wait`. Writers take it after the account's state lock; the server
    /// never takes it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Locked`] when another holder kept it for all of `wait`,
    /// [`StoreError::Io`] when the lock file cannot be opened or locked.
    pub fn lock_bindings(&self, wait: Duration) -> Result<BindingsLock, StoreError> {
        BindingsLock::acquire(&self.root.join(BINDINGS_LOCK_NAME), wait)
    }

    /// The single-account state file of earlier versions, directly in the
    /// state directory.
    #[must_use]
    pub fn legacy_state_file(&self) -> PathBuf {
        self.root.join(STATE_FILE_NAME)
    }

    /// Creates the directory of `name` (mode 0700) when it is missing. The
    /// state directory itself must already exist.
    ///
    /// # Errors
    ///
    /// The I/O error when the directory cannot be created, or when the path
    /// exists and is not a directory.
    pub fn ensure_dir(&self, name: &AccountName) -> io::Result<()> {
        let path = self.root.join(name.as_str());
        match std::fs::DirBuilder::new()
            .mode(ACCOUNT_DIR_MODE)
            .create(&path)
        {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if std::fs::symlink_metadata(&path)?.is_dir() {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "the account path exists and is not a directory",
                    ))
                }
            }
            Err(err) => Err(err),
        }
    }
}

/// A Peloton user id as it may appear in logs and seed output: an ellipsis and
/// the last four characters.
#[must_use]
pub fn user_suffix(id: &PelotonId) -> String {
    let text = id.to_string();
    let tail = text
        .len()
        .checked_sub(SUFFIX_LEN)
        .and_then(|start| text.get(start..))
        .unwrap_or(&text);
    format!("\u{2026}{tail}")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use peloton::ids::PelotonId;

    use super::{AccountName, Accounts, AccountsError, InvalidAccountName, user_suffix};

    fn name(text: &str) -> AccountName {
        text.parse().unwrap()
    }

    #[test]
    fn account_names_follow_the_pattern() {
        for good in ["a", "alice", "abcdefghijklmnop"] {
            assert_eq!(good.parse::<AccountName>().unwrap().as_str(), good);
        }
        for bad in [
            "",
            "Alice",
            "al1ce",
            "al-ice",
            "abcdefghijklmnopq",
            " alice",
            "\u{e9}",
        ] {
            assert_eq!(
                bad.parse::<AccountName>(),
                Err(InvalidAccountName),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn accounts_must_be_non_empty_and_distinct() {
        let root = std::path::PathBuf::from("/state");
        assert_eq!(
            Accounts::new(root.clone(), vec![]).unwrap_err(),
            AccountsError::Empty
        );
        assert_eq!(
            Accounts::new(root.clone(), vec![name("alice"), name("alice")]).unwrap_err(),
            AccountsError::Duplicate
        );
        let accounts = Accounts::new(root, vec![name("bob"), name("alice")]).unwrap();
        assert_eq!(accounts.names(), [name("bob"), name("alice")]);
        assert!(accounts.contains(&name("alice")));
        assert!(!accounts.contains(&name("carol")));
        assert_eq!(accounts.root().to_str(), Some("/state"));
        assert_eq!(
            accounts.state_file(&name("bob")).path.to_str(),
            Some("/state/bob/state.json")
        );
        assert_eq!(
            accounts.legacy_state_file().to_str(),
            Some("/state/state.json")
        );
        let peers = accounts.peers_of(&name("bob"));
        assert_eq!(
            peers
                .iter()
                .map(|(peer, file)| (peer.as_str(), file.path.to_str()))
                .collect::<Vec<_>>(),
            [("alice", Some("/state/alice/state.json"))]
        );
    }

    #[test]
    fn ensure_dir_creates_a_private_directory() {
        let root = tempfile::tempdir().unwrap();
        let accounts =
            Accounts::new(root.path().to_owned(), vec![name("alice"), name("bob")]).unwrap();
        accounts.ensure_dir(&name("alice")).unwrap();
        let dir = root.path().join("alice");
        assert!(dir.is_dir());
        assert_eq!(dir.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        accounts.ensure_dir(&name("alice")).unwrap();
        std::fs::write(root.path().join("bob"), b"").unwrap();
        assert!(accounts.ensure_dir(&name("bob")).is_err());
    }

    #[test]
    fn ensure_dir_never_creates_the_root() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let accounts = Accounts::new(missing.clone(), vec![name("alice")]).unwrap();
        let err = accounts.ensure_dir(&name("alice")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!missing.exists());
    }

    #[test]
    fn user_suffix_is_the_last_four_hex() {
        let id: PelotonId = "000000000000000000000000000000f2".parse().unwrap();
        assert_eq!(user_suffix(&id), "\u{2026}00f2");
    }
}
