//! `capture`: save raw Peloton responses to disk for fixture building.

use std::fs::{self, DirBuilder};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use peloton::endpoints;
use peloton::ids::PelotonId;
use rustix::fs::{Mode, OFlags};
use secrecy::SecretString;
use serde::Deserialize;

use crate::check::{MeShapeError, parse_me};
use crate::session::{Session, SessionError};

/// Why a capture directory could not be chosen, prepared or written to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The directory is inside a git work tree, where raw data must not go.
    #[error(
        "refusing to write captures inside the git work tree {}; use <work tree>/captures or a directory outside it",
        .root.display()
    )]
    InsideWorktree {
        /// The root of the work tree.
        root: PathBuf,
    },
    /// The directory could not be made absolute or canonical.
    #[error("bad --dir")]
    Resolve(#[source] std::io::Error),
    /// The directory has no usable path components, for example a `..` in the
    /// part that does not exist yet.
    #[error("bad --dir")]
    Unresolvable,
    /// The directory could not be created.
    #[error("could not create {}", .path.display())]
    Create {
        /// The directory.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// A file could not be written.
    #[error("could not write {}", .path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The file to write is not a regular file with a single link (a
    /// symbolic link, a hard link, a FIFO, …), so it could redirect the
    /// capture or have another file truncated.
    #[error("refusing to write {}: not a regular file with a single link", .path.display())]
    NotPrivate {
        /// The file.
        path: PathBuf,
    },
    /// Permissions could not be restricted to the owner.
    #[error("could not secure {}", .path.display())]
    Secure {
        /// The file or directory.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}

/// The workouts-list response was not a workouts list. The parse error is
/// dropped on purpose: it could echo parts of the body.
#[derive(Debug, thiserror::Error)]
#[error("unexpected workouts list shape")]
pub struct WorkoutsShapeError;

/// Why `capture` failed. Never carries tokens or bodies.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CaptureError {
    /// The directory was refused or a file could not be saved.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Login or a required request failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// `/me` answered with something unexpected.
    #[error(transparent)]
    Me(#[from] MeShapeError),
    /// The workouts list answered with something unexpected.
    #[error(transparent)]
    Workouts(#[from] WorkoutsShapeError),
    /// Some per-workout requests failed; each was reported when it happened.
    #[error("some requests failed")]
    Incomplete,
}

const JOINS: &str = "joins=ride,ride.instructor";

#[derive(Deserialize)]
struct WorkoutList {
    data: Vec<WorkoutRef>,
}

#[derive(Deserialize)]
struct WorkoutRef {
    id: String,
}

/// The workout ids of a workouts-list response.
#[derive(Debug, PartialEq, Eq)]
pub struct WorkoutIds {
    /// The valid ids, in list order.
    pub valid: Vec<PelotonId>,
    /// How many entries were skipped because their id is not a valid
    /// [`PelotonId`].
    pub skipped: usize,
}

/// Extracts and validates the workout ids of a workouts-list response.
///
/// Entries whose id is not a valid [`PelotonId`] are counted, not returned;
/// the ids themselves are not reported, as they are untrusted.
///
/// # Errors
///
/// Returns [`WorkoutsShapeError`] when the body is not a workouts list.
pub fn workout_ids(body: &[u8]) -> Result<WorkoutIds, WorkoutsShapeError> {
    let list: WorkoutList = serde_json::from_slice(body).map_err(|_| WorkoutsShapeError)?;
    let valid: Vec<PelotonId> = list.data.iter().filter_map(|w| w.id.parse().ok()).collect();
    let skipped = list.data.len() - valid.len();
    Ok(WorkoutIds { valid, skipped })
}

/// Extracts the ride id (`ride.id`) from a workout response, if valid.
#[must_use]
pub fn ride_id(workout: &[u8]) -> Option<PelotonId> {
    let value: serde_json::Value = serde_json::from_slice(workout).ok()?;
    value.pointer("/ride/id")?.as_str()?.parse().ok()
}

/// Refuses `target` when it is inside a git work tree and is not exactly
/// `<work tree>/captures`. Both paths must already be canonical.
///
/// # Errors
///
/// Returns [`StoreError::InsideWorktree`] when the directory would land in the
/// work tree.
pub fn guard_dir(target: &Path, worktree: Option<&Path>) -> Result<(), StoreError> {
    match worktree {
        Some(root) if target != root.join("captures") => Err(StoreError::InsideWorktree {
            root: root.to_path_buf(),
        }),
        _ => Ok(()),
    }
}

/// Resolves `dir` to an absolute path without creating anything: the deepest
/// existing ancestor is canonicalized and the remaining components appended.
///
/// # Errors
///
/// Returns [`StoreError`] when the path cannot be resolved or contains `..` in
/// the part that does not exist yet.
pub fn resolve_dir(dir: &Path) -> Result<PathBuf, StoreError> {
    let absolute = std::path::absolute(dir).map_err(StoreError::Resolve)?;
    let mut existing = absolute.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or(StoreError::Unresolvable)?;
        rest.push(name);
        existing = existing.parent().ok_or(StoreError::Unresolvable)?;
    }
    let mut resolved = existing.canonicalize().map_err(StoreError::Resolve)?;
    for name in rest.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// Finds the root of the git work tree containing `path`, if any.
#[must_use]
pub fn find_worktree(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}

pub(crate) fn prepare_dir(dir: &Path) -> Result<(), StoreError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|source| StoreError::Create {
            path: dir.to_path_buf(),
            source,
        })?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
        StoreError::Secure {
            path: dir.to_path_buf(),
            source,
        }
    })
}

/// Opens `path` for writing, empty, with mode 0600 before any content is
/// written: new files are created with it, and an existing file is
/// restricted before it is truncated.
///
/// A symbolic link is refused (`O_NOFOLLOW`), and so is anything but a
/// regular file with a single link, so a planted link cannot redirect the
/// capture or have another file truncated and re-permissioned. The file is
/// opened with `O_NONBLOCK`, so a planted FIFO cannot block the open, and
/// switched back to blocking once it is known to be a regular file.
fn create_private(path: &Path) -> Result<fs::File, StoreError> {
    let write_error = |source: std::io::Error| StoreError::Write {
        path: path.to_path_buf(),
        source,
    };
    let flags =
        OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = fs::File::from(
        rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR)
            .map_err(|errno| write_error(errno.into()))?,
    );
    let meta = file.metadata().map_err(write_error)?;
    if !meta.is_file() || meta.nlink() > 1 {
        return Err(StoreError::NotPrivate {
            path: path.to_path_buf(),
        });
    }
    let status = rustix::fs::fcntl_getfl(&file).map_err(|errno| write_error(errno.into()))?;
    rustix::fs::fcntl_setfl(&file, status - OFlags::NONBLOCK)
        .map_err(|errno| write_error(errno.into()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| StoreError::Secure {
            path: path.to_path_buf(),
            source,
        })?;
    file.set_len(0).map_err(write_error)?;
    Ok(file)
}

pub(crate) fn save(dir: &Path, name: &str, body: &[u8]) -> Result<(), StoreError> {
    let path = dir.join(name);
    let mut file = create_private(&path)?;
    file.write_all(body).map_err(|source| StoreError::Write {
        path: path.clone(),
        source,
    })?;
    eprintln!("saved {name}");
    Ok(())
}

/// Logs in and saves `/me`, the first `count` workouts and their details.
///
/// # Errors
///
/// Returns a [`CaptureError`] without secrets when any step fails.
pub fn run(session: &Session, dir: &Path, count: u32) -> Result<(), CaptureError> {
    let dir = resolve_dir(dir)?;
    guard_dir(&dir, find_worktree(&dir).as_deref())?;
    prepare_dir(&dir)?;

    let tokens = session.login()?;
    let access = &tokens.access_token;

    let me_body = session.get(access, &endpoints::me())?;
    let me = parse_me(&me_body)?;
    save(&dir, "me.json", &me_body)?;

    session.pause();
    let list = session.get(
        access,
        &format!(
            "{}?limit={count}&page=0&sort_by=-created&{JOINS}",
            endpoints::user_workouts(&me.id)
        ),
    )?;
    save(&dir, &format!("workouts-{}.json", me.id), &list)?;

    let mut tally = Tally::default();
    // me.json and the workouts list are already saved.
    tally.saved += 2;
    let ids = workout_ids(&list)?;
    if ids.skipped > 0 {
        eprintln!(
            "skipped {} workout(s) in the list with an invalid id",
            ids.skipped
        );
    }
    for id in ids.valid {
        let fetch = |query: String, name: String, tally: &mut Tally| {
            session.pause();
            fetch_and_save(session, access, &dir, &query, &name, tally)
        };
        let workout = fetch(
            format!("{}?{JOINS}", endpoints::workout(&id)),
            format!("workout-{id}.json"),
            &mut tally,
        )?;
        fetch(
            format!("{}?every_n=1", endpoints::performance_graph(&id)),
            format!("performance_graph-{id}.json"),
            &mut tally,
        )?;
        let Some(workout) = workout else { continue };
        if let Some(ride) = ride_id(&workout) {
            fetch(
                endpoints::ride_details(&ride),
                format!("ride_details-{ride}.json"),
                &mut tally,
            )?;
        } else {
            eprintln!("workout {id}: no valid ride id, skipping ride details");
        }
    }
    eprintln!("{}", tally.summary());
    if tally.failed > 0 {
        return Err(CaptureError::Incomplete);
    }
    Ok(())
}

/// Counts of saved and failed per-workout requests.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    saved: u32,
    failed: u32,
}

impl Tally {
    fn summary(&self) -> String {
        format!("{} saved, {} failed", self.saved, self.failed)
    }
}

/// Fetches `query` and saves it as `name`. A failed request is reported on
/// stderr (path and status only) and counted; only a failed write aborts.
fn fetch_and_save(
    session: &Session,
    access: &SecretString,
    dir: &Path,
    query: &str,
    name: &str,
    tally: &mut Tally,
) -> Result<Option<Vec<u8>>, StoreError> {
    match session.get(access, query) {
        Ok(body) => {
            save(dir, name, &body)?;
            tally.saved += 1;
            Ok(Some(body))
        }
        Err(error) => {
            eprintln!("failed: {}", crate::report(&error));
            tally.failed += 1;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::{
        StoreError, Tally, create_private, guard_dir, prepare_dir, resolve_dir, ride_id, save,
        workout_ids,
    };
    use crate::report;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn guard_allows_outside_worktree() {
        assert!(guard_dir(Path::new("/data/caps"), None).is_ok());
    }

    #[test]
    fn guard_allows_exactly_captures() {
        let root = Path::new("/repo");
        assert!(guard_dir(Path::new("/repo/captures"), Some(root)).is_ok());
    }

    #[test]
    fn guard_refuses_other_paths_in_worktree() {
        let root = Path::new("/repo");
        assert!(guard_dir(Path::new("/repo/crates"), Some(root)).is_err());
        assert!(guard_dir(Path::new("/repo/captures/sub"), Some(root)).is_err());
        assert!(guard_dir(Path::new("/repo"), Some(root)).is_err());
    }

    #[test]
    fn guard_refusal_names_the_work_tree() {
        let error = guard_dir(Path::new("/repo/crates"), Some(Path::new("/repo")))
            .expect_err("inside the work tree");
        assert_eq!(
            report(&error),
            "refusing to write captures inside the git work tree /repo; use <work tree>/captures or a directory outside it"
        );
    }

    #[test]
    fn resolve_refuses_parent_components_that_do_not_exist() {
        let error = resolve_dir(Path::new("/tmp/peloton-relay-login-absent/../x"))
            .expect_err("parent component");
        assert!(matches!(error, StoreError::Unresolvable));
        assert_eq!(report(&error), "bad --dir");
    }

    #[test]
    fn save_failure_reports_the_path_then_the_io_cause() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent");
        let error = save(&missing, "a.json", b"{}").expect_err("no such directory");
        assert!(matches!(error, StoreError::Write { .. }));
        let text = report(&error);
        assert!(
            text.starts_with(&format!(
                "could not write {}: ",
                missing.join("a.json").display()
            )),
            "{text}"
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn prepare_failure_reports_the_directory_then_the_io_cause() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("file");
        std::fs::write(&file, b"").expect("file");
        let target = file.join("sub");
        let error = prepare_dir(&target).expect_err("parent is a file");
        assert!(matches!(error, StoreError::Create { .. }));
        assert!(report(&error).starts_with(&format!("could not create {}: ", target.display())));
    }

    #[test]
    fn workouts_shape_failure_does_not_echo_the_body() {
        let error = workout_ids(br#"{"data":"secret"}"#).expect_err("not a list");
        assert_eq!(report(&error), "unexpected workouts list shape");
    }

    #[test]
    fn ride_id_extracts_valid_only() {
        let ok = format!(r#"{{"ride":{{"id":"{ID}"}}}}"#);
        assert_eq!(ride_id(ok.as_bytes()).expect("id").to_string(), ID);
        assert!(ride_id(br#"{"ride":{"id":"0"}}"#).is_none());
        assert!(ride_id(br#"{"ride":null}"#).is_none());
        assert!(ride_id(br"{}").is_none());
    }

    #[test]
    fn workout_ids_report_the_invalid_ones_they_skip() {
        let body = format!(r#"{{"data":[{{"id":"{ID}"}},{{"id":"../x"}},{{"id":"0"}}]}}"#);
        let ids = workout_ids(body.as_bytes()).expect("list");
        assert_eq!(ids.valid.len(), 1);
        assert_eq!(ids.skipped, 2);
    }

    #[test]
    fn create_private_restricts_an_existing_file_before_it_is_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("me.json");
        fs::write(&path, b"old and readable").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let file = create_private(&path).expect("open");

        let meta = file.metadata().expect("metadata");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.len(), 0);
    }

    #[test]
    fn create_private_refuses_a_symlink_and_leaves_its_target_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target.txt");
        fs::write(&target, b"keep me").expect("seed");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("chmod");
        let link = dir.path().join("me.json");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        assert!(create_private(&link).is_err());

        assert_eq!(fs::read(&target).expect("read"), b"keep me");
        let mode = fs::metadata(&target)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn create_private_refuses_a_hard_linked_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target.txt");
        fs::write(&target, b"keep me").expect("seed");
        let link = dir.path().join("me.json");
        fs::hard_link(&target, &link).expect("hard link");

        assert!(matches!(
            create_private(&link),
            Err(StoreError::NotPrivate { .. })
        ));

        assert_eq!(fs::read(&target).expect("read"), b"keep me");
    }

    #[test]
    fn create_private_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("me.json");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "mkfifo failed");

        let (sender, receiver) = std::sync::mpsc::channel();
        let opened = path.clone();
        std::thread::spawn(move || {
            let refused = create_private(&opened).is_err();
            sender.send(refused).expect("send result");
        });
        let refused = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("create_private blocked on a FIFO");

        assert!(refused);
    }

    #[test]
    fn create_private_creates_new_files_as_0600() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("new.json");

        create_private(&path).expect("open");

        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn tally_summary_counts() {
        let tally = Tally {
            saved: 7,
            failed: 2,
        };
        assert_eq!(tally.summary(), "7 saved, 2 failed");
    }
}
