//! Crash-safe persistence of the Peloton credential.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// The file exists but is empty or not a valid state document.
    #[error("state file is empty or corrupt")]
    Corrupt,
    /// The file system refused the operation.
    #[error("state file I/O failed")]
    Io(#[source] io::Error),
}

/// Distinguishes temp files of concurrent saves within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(StoreError::Io(err)),
        };
        // The parse error is dropped on purpose: its message can quote file content.
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| StoreError::Corrupt)
    }

    /// Atomically replaces the state file.
    ///
    /// Writes a temp file (mode 0600 from creation) in the same directory,
    /// syncs it, renames it over the target and syncs the directory, so a crash
    /// leaves either the old or the new state, never a partial one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when any step fails; the target is then unchanged.
    pub fn save(&self, state: &State) -> Result<(), StoreError> {
        let dir = self.dir()?;
        let json = serde_json::to_vec(state).map_err(|err| StoreError::Io(err.into()))?;
        let temp = self.temp_path(dir)?;

        let result = write_synced(&temp, &json)
            .and_then(|()| fs::rename(&temp, &self.path))
            .and_then(|()| File::open(dir)?.sync_all());
        if result.is_err() {
            // Best effort: the temp file may not exist, and the original error matters more.
            let _ = fs::remove_file(&temp);
        }
        result.map_err(StoreError::Io)
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

    fn temp_path(&self, dir: &Path) -> Result<PathBuf, StoreError> {
        let name = self.path.file_name().ok_or_else(|| {
            StoreError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state path has no file name",
            ))
        })?;
        let mut temp = std::ffi::OsString::from(".");
        temp.push(name);
        temp.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        Ok(dir.join(temp))
    }
}

/// Creates `path` with mode 0600, writes `contents` and syncs it to disk.
fn write_synced(path: &Path, contents: &[u8]) -> io::Result<()> {
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
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::write_synced;

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
