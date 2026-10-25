//! `capture`: save raw Peloton responses to disk for fixture building.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use peloton::endpoints;
use peloton::ids::PelotonId;
use secrecy::SecretString;
use serde::Deserialize;

use crate::check::parse_me;
use crate::session::Session;

const JOINS: &str = "joins=ride,ride.instructor";

#[derive(Deserialize)]
struct WorkoutList {
    data: Vec<WorkoutRef>,
}

#[derive(Deserialize)]
struct WorkoutRef {
    id: String,
}

/// Extracts and validates the workout ids of a workouts-list response.
///
/// Entries whose id is not a valid [`PelotonId`] are dropped.
///
/// # Errors
///
/// Returns a message when the body is not a workouts list.
pub fn workout_ids(body: &[u8]) -> Result<Vec<PelotonId>, String> {
    let list: WorkoutList =
        serde_json::from_slice(body).map_err(|_| "unexpected workouts list shape".to_owned())?;
    Ok(list.data.iter().filter_map(|w| w.id.parse().ok()).collect())
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
/// Returns a message when the directory would land in the work tree.
pub fn guard_dir(target: &Path, worktree: Option<&Path>) -> Result<(), String> {
    match worktree {
        Some(root) if target != root.join("captures") => Err(format!(
            "refusing to write captures inside the git work tree {}; use <work tree>/captures or a directory outside it",
            root.display()
        )),
        _ => Ok(()),
    }
}

/// Resolves `dir` to an absolute path without creating anything: the deepest
/// existing ancestor is canonicalized and the remaining components appended.
///
/// # Errors
///
/// Returns a message when the path cannot be resolved or contains `..` in the
/// part that does not exist yet.
pub fn resolve_dir(dir: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(dir).map_err(|e| format!("bad --dir: {e}"))?;
    let mut existing = absolute.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or("bad --dir")?;
        rest.push(name);
        existing = existing.parent().ok_or("bad --dir")?;
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|e| format!("bad --dir: {e}"))?;
    for name in rest.into_iter().rev() {
        if Path::new(name)
            .components()
            .any(|c| c == Component::ParentDir)
        {
            return Err("--dir must not contain '..' in components that do not exist".to_owned());
        }
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

pub(crate) fn prepare_dir(dir: &Path) -> Result<(), String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("could not secure {}: {e}", dir.display()))
}

pub(crate) fn save(dir: &Path, name: &str, body: &[u8]) -> Result<(), String> {
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    file.write_all(body)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("could not secure {}: {e}", path.display()))?;
    eprintln!("saved {name}");
    Ok(())
}

/// Logs in and saves `/me`, the first `count` workouts and their details.
///
/// # Errors
///
/// Returns a message without secrets when any step fails.
pub fn run(session: &Session, dir: &Path, count: u32) -> Result<(), String> {
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
    for id in workout_ids(&list)? {
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
        return Err("some requests failed".to_owned());
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
) -> Result<Option<Vec<u8>>, String> {
    match session.get(access, query) {
        Ok(body) => {
            save(dir, name, &body)?;
            tally.saved += 1;
            Ok(Some(body))
        }
        Err(message) => {
            eprintln!("failed: {message}");
            tally.failed += 1;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Tally, guard_dir, ride_id, workout_ids};

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
    fn ride_id_extracts_valid_only() {
        let ok = format!(r#"{{"ride":{{"id":"{ID}"}}}}"#);
        assert_eq!(ride_id(ok.as_bytes()).expect("id").to_string(), ID);
        assert!(ride_id(br#"{"ride":{"id":"0"}}"#).is_none());
        assert!(ride_id(br#"{"ride":null}"#).is_none());
        assert!(ride_id(br"{}").is_none());
    }

    #[test]
    fn workout_ids_drop_invalid() {
        let body = format!(r#"{{"data":[{{"id":"{ID}"}},{{"id":"../x"}}]}}"#);
        assert_eq!(workout_ids(body.as_bytes()).expect("list").len(), 1);
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
