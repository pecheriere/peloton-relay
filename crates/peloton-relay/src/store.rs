//! Crash-safe persistence of the Peloton credential.

use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use peloton::ids::PelotonId;
use peloton::oauth::TokenSet;
use serde::{Deserialize, Serialize};

/// Everything the relay needs to call Peloton on the owner's behalf.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// Current access and refresh tokens.
    pub tokens: TokenSet,
    /// The owner's Peloton user id.
    pub user_id: PelotonId,
}

/// Why the state file could not be read or written.
///
/// Never carries file contents: the file holds the credential.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The file exists but is empty or not a valid document.
    #[error("state or binding file is empty or corrupt")]
    Corrupt,
    /// The file system refused the operation.
    #[error("state or binding file I/O failed")]
    Io(#[source] io::Error),
    /// Another writer held the lock for the whole wait.
    #[error("state or binding lock is held by another writer")]
    Locked,
}

/// The file name of the binding, next to the state file.
const BINDING_FILE_NAME: &str = "binding.json";

/// Which Peloton user an account is bound to.
///
/// Written once by the first seed of the account; it keeps a credential for
/// another Peloton user from being saved into the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// The bound Peloton user.
    pub user_id: PelotonId,
}

/// Distinguishes temp files of concurrent saves within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temp file untouched for this long belongs to a writer that crashed.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60);

/// How often [`StateFile::lock`] retries a lock another writer holds.
const LOCK_POLL: Duration = Duration::from_millis(50);

/// Identifies one version of the state file.
///
/// Every save renames a new inode into place, so inode plus size and mtime
/// tell an external replacement apart from the file this process last saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
}

impl FileId {
    fn of(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        }
    }
}

/// One read of the state file.
#[derive(Debug)]
pub(crate) enum Snapshot {
    Missing,
    Corrupt(FileId),
    Valid(State, FileId),
}

/// The JSON file holding the [`State`].
#[derive(Debug, Clone)]
pub struct StateFile {
    /// Location of the state file; its directory must exist.
    pub path: PathBuf,
}

impl StateFile {
    /// Reads the state.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] when the file is empty or does not parse,
    /// [`StoreError::Io`] when it cannot be read. A missing file is `Ok(None)`.
    pub fn load(&self) -> Result<Option<State>, StoreError> {
        match self.snapshot().map_err(StoreError::Io)? {
            Snapshot::Missing => Ok(None),
            Snapshot::Corrupt(_) => Err(StoreError::Corrupt),
            Snapshot::Valid(state, _) => Ok(Some(state)),
        }
    }

    /// Reads the state together with the identity of the version read.
    ///
    /// Takes no lock: the file is only ever replaced by a rename, so a read
    /// sees one whole version. A corrupt file is a [`Snapshot`], not an error.
    pub(crate) fn snapshot(&self) -> io::Result<Snapshot> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Snapshot::Missing),
            Err(err) => return Err(err),
        };
        let id = FileId::of(&file.metadata()?);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        // The parse error is dropped on purpose: its message can quote file content.
        Ok(match serde_json::from_slice(&bytes) {
            Ok(state) => Snapshot::Valid(state, id),
            Err(_) => Snapshot::Corrupt(id),
        })
    }

    /// The identity of the current file version, `None` if there is none.
    pub(crate) fn current_id(&self) -> io::Result<Option<FileId>> {
        match fs::metadata(&self.path) {
            Ok(meta) => Ok(Some(FileId::of(&meta))),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Takes the exclusive lock every writer of the state file holds, waiting
    /// at most `wait` for another holder to release it.
    ///
    /// The lock is an advisory `flock` on `.<name>.lock` next to the state
    /// file, created with mode 0600 and never removed, so every writer locks
    /// the same inode. It keeps the relay's refresh (from re-reading the file
    /// to saving the rotated tokens) and `seed` in another process apart. It
    /// is released when the returned [`StateLock`] and its clones are dropped,
    /// or when the process exits. Blocks the calling thread while it waits.
    ///
    /// # Errors
    ///
    /// [`StoreError::Locked`] when another holder kept the lock for all of
    /// `wait`, [`StoreError::Io`] when the lock file cannot be opened or locked.
    pub fn lock(&self, wait: Duration) -> Result<StateLock, StoreError> {
        let path = self.dir()?.join(lock_name(self.file_name()?));
        let file = exclusive_lock(&path, wait)?;
        Ok(StateLock {
            store: self.clone(),
            _file: Arc::new(file),
        })
    }

    /// Takes the lock, waiting at most `wait` (see [`Self::lock`]), and
    /// atomically replaces the state file (see [`StateLock::save`]).
    ///
    /// # Errors
    ///
    /// As [`Self::lock`] and [`StateLock::save`]; the target is then unchanged.
    pub fn save(&self, state: &State, wait: Duration) -> Result<(), StoreError> {
        self.lock(wait)?.save(state)
    }

    /// Atomically replaces the state file; the caller holds the lock.
    fn replace(&self, state: &State) -> Result<FileId, StoreError> {
        let json = serde_json::to_vec(state).map_err(|err| StoreError::Io(err.into()))?;
        write_atomic(&self.path, &json)
    }

    /// The binding file, next to the state file.
    #[must_use]
    pub fn binding_path(&self) -> PathBuf {
        self.path.with_file_name(BINDING_FILE_NAME)
    }

    /// Reads the binding, taking no lock: the file is only ever replaced by a
    /// rename, so a read sees one whole version.
    ///
    /// [`Self::load`] does not check the binding; callers that must ensure the
    /// state belongs to the bound user compare them.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] when the file does not parse or has unknown
    /// keys, [`StoreError::Io`] when it cannot be read. A missing file is
    /// `Ok(None)`.
    pub fn read_binding(&self) -> Result<Option<Binding>, StoreError> {
        let bytes = match fs::read(self.binding_path()) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(StoreError::Io(err)),
        };
        // The parse error is dropped on purpose: its message can quote file content.
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| StoreError::Corrupt)
    }

    fn dir(&self) -> Result<&Path, StoreError> {
        match self.path.parent() {
            Some(dir) if dir.as_os_str().is_empty() => Ok(Path::new(".")),
            Some(dir) => Ok(dir),
            None => Err(StoreError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state path has no parent directory",
            ))),
        }
    }

    fn file_name(&self) -> Result<&OsStr, StoreError> {
        self.path.file_name().ok_or_else(|| {
            StoreError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state path has no file name",
            ))
        })
    }
}

/// Exclusive hold on the state file lock, from [`StateFile::lock`].
///
/// Clones share the hold; it ends when the last one is dropped.
#[derive(Debug, Clone)]
pub struct StateLock {
    store: StateFile,
    /// Holds the `flock`; closing the last handle releases it.
    _file: Arc<File>,
}

impl StateLock {
    /// Atomically replaces the state file.
    ///
    /// Writes a temp file (mode 0600 from creation) in the same directory,
    /// syncs it, renames it over the target and syncs the directory, so a crash
    /// leaves either the old or the new state, never a partial one. Afterwards
    /// removes temp files that crashed writers left behind.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when any step fails; the target is then unchanged.
    pub fn save(&self, state: &State) -> Result<(), StoreError> {
        self.store.replace(state).map(|_| ())
    }

    /// The binding of the account, read under the lock (see
    /// [`StateFile::read_binding`]).
    ///
    /// # Errors
    ///
    /// As [`StateFile::read_binding`].
    pub fn binding(&self) -> Result<Option<Binding>, StoreError> {
        self.store.read_binding()
    }

    /// Atomically binds the account to `user`, the way [`Self::save`] writes
    /// the state: private temp file, sync, rename, directory sync. Writers
    /// call this before `save`, both under this lock.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when any step fails; the binding is then unchanged.
    pub fn bind(&self, user: &PelotonId) -> Result<(), StoreError> {
        let json = serde_json::to_vec(&Binding {
            user_id: user.clone(),
        })
        .map_err(|err| StoreError::Io(err.into()))?;
        write_atomic(&self.store.binding_path(), &json).map(|_| ())
    }

    /// [`Self::save`], returning the identity of the version written.
    pub(crate) fn save_with_id(&self, state: &State) -> Result<FileId, StoreError> {
        self.store.replace(state)
    }
}

/// Exclusive hold on `<root>/.bindings.lock`, from
/// [`Accounts::lock_bindings`](crate::account::Accounts::lock_bindings).
///
/// Serialises the check that a Peloton user is bound to one account only.
/// Released when dropped or when the process exits.
#[derive(Debug)]
pub struct BindingsLock {
    /// Holds the `flock`; closing it releases the lock.
    _file: File,
}

impl BindingsLock {
    /// Takes the lock at `path`, waiting at most `wait`.
    pub(crate) fn acquire(path: &Path, wait: Duration) -> Result<Self, StoreError> {
        exclusive_lock(path, wait).map(|file| Self { _file: file })
    }
}

/// Opens (creating it with mode 0600, never removing it) the lock file at
/// `path` and takes an exclusive advisory `flock`, polling for at most `wait`.
/// Blocks the calling thread while it waits.
fn exclusive_lock(path: &Path, wait: Duration) -> Result<File, StoreError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(StoreError::Io)?;
    // `None`: the wait does not fit in an `Instant`, so it never ends.
    let deadline = Instant::now().checked_add(wait);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(err)) => return Err(StoreError::Io(err)),
        }
        let pause = match deadline {
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => left.min(LOCK_POLL),
                _ => return Err(StoreError::Locked),
            },
            None => LOCK_POLL,
        };
        thread::sleep(pause);
    }
}

/// Atomically replaces the file at `path` with `bytes`; the caller holds the
/// lock.
///
/// Writes a private temp file in the same directory, syncs it, renames it
/// over the target and syncs the directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<FileId, StoreError> {
    let invalid =
        |what: &'static str| StoreError::Io(io::Error::new(io::ErrorKind::InvalidInput, what));
    let dir = match path.parent() {
        Some(dir) if dir.as_os_str().is_empty() => Path::new("."),
        Some(dir) => dir,
        None => return Err(invalid("path has no parent directory")),
    };
    let name = path
        .file_name()
        .ok_or_else(|| invalid("path has no file name"))?;
    let temp = dir.join(temp_name(name));

    // Identity comes from the temp file's handle: rename keeps the inode and
    // mtime, and a stat of the path afterwards could see another writer's file.
    let result = write_synced(&temp, bytes).and_then(|id| {
        fs::rename(&temp, path)?;
        File::open(dir)?.sync_all()?;
        Ok(id)
    });
    match result {
        Ok(id) => {
            remove_stale_temps(dir, name);
            Ok(id)
        }
        Err(err) => {
            // Best effort: the temp file may not exist, and the original error matters more.
            let _ = fs::remove_file(&temp);
            Err(StoreError::Io(err))
        }
    }
}

/// `.<name>.lock`: the lock file of the state file `name`.
fn lock_name(name: &OsStr) -> std::ffi::OsString {
    let mut lock = std::ffi::OsString::from(".");
    lock.push(name);
    lock.push(".lock");
    lock
}

/// `.<name>.<pid>.<counter>.tmp`: unique among live writers.
fn temp_name(name: &OsStr) -> std::ffi::OsString {
    let mut temp = std::ffi::OsString::from(".");
    temp.push(name);
    temp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    temp
}

/// Whether `candidate` has exactly the shape [`temp_name`] gives `name`.
fn is_temp_name(name: &OsStr, candidate: &OsStr) -> bool {
    let (Some(name), Some(candidate)) = (name.to_str(), candidate.to_str()) else {
        return false;
    };
    let Some(middle) = candidate
        .strip_prefix('.')
        .and_then(|rest| rest.strip_prefix(name))
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|rest| rest.strip_suffix(".tmp"))
    else {
        return false;
    };
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    matches!(middle.split_once('.'), Some((pid, counter)) if is_number(pid) && is_number(counter))
}

/// Removes temp files of `name` that no writer has touched for a while.
///
/// Young temp files may belong to a live writer in another process (`seed`)
/// and are left alone. Failures only leave clutter in a private directory, so
/// they are logged, not returned.
fn remove_stale_temps(dir: &Path, name: &OsStr) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(event = "state_temp_cleanup_failed", cause = ?err.kind(), "cannot list the state directory");
            return;
        }
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        if !is_temp_name(name, &entry.file_name()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age >= STALE_TEMP_AGE)
            });
        if stale && let Err(err) = fs::remove_file(entry.path()) {
            tracing::warn!(event = "state_temp_cleanup_failed", cause = ?err.kind(), "cannot remove a stale temp file");
        }
    }
}

/// Creates `path` with mode 0600, writes `contents` and syncs it to disk.
fn write_synced(path: &Path, contents: &[u8]) -> io::Result<FileId> {
    let open = || {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    let mut file = match open() {
        // The name is unique among live writers (pid + counter), so an existing
        // file is a leftover of a crashed process that had the same pid.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            open()?
        }
        other => other?,
    };
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(FileId::of(&file.metadata()?))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use std::ffi::OsStr;

    use super::{is_temp_name, lock_name, temp_name, write_synced};

    #[test]
    fn temp_names_match_only_their_own_pattern() {
        let name = OsStr::new("state.json");
        assert!(is_temp_name(name, &temp_name(name)));
        assert!(is_temp_name(name, OsStr::new(".state.json.1.0.tmp")));
        for other in [
            "state.json",
            ".state.json",
            ".state.json.tmp",
            ".state.json.1.tmp",
            ".state.json.1..tmp",
            ".state.json.a.0.tmp",
            ".state.json.1.0.1.tmp",
            ".state.json.1.0.tmp.bak",
            "state.json.1.0.tmp",
            ".other.json.1.0.tmp",
            ".state.jsonx.1.0.tmp",
        ] {
            assert!(!is_temp_name(name, OsStr::new(other)), "{other}");
        }
        // Stale temp cleanup never removes the lock file.
        assert!(!is_temp_name(name, &lock_name(name)));
    }

    #[test]
    fn state_temp_cleanup_never_matches_the_binding_or_lock_files() {
        let state = OsStr::new("state.json");
        let binding = OsStr::new("binding.json");
        assert!(!is_temp_name(state, binding));
        assert!(!is_temp_name(binding, &temp_name(state)));
        assert!(is_temp_name(binding, &temp_name(binding)));
        assert!(!is_temp_name(state, &temp_name(binding)));
        assert!(!is_temp_name(state, OsStr::new(".bindings.lock")));
        assert!(!is_temp_name(binding, OsStr::new(".bindings.lock")));
        assert!(!is_temp_name(state, &lock_name(state)));
    }

    #[test]
    fn write_synced_replaces_stale_temp_file_with_private_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".state.json.1.0.tmp");
        fs::write(&path, b"stale").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        write_synced(&path, b"fresh").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"fresh");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
