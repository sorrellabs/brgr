//! Owner-session worker counts for a harness status line.
//!
//! Writers re-project every bound session into `$BRGR_HOME/statusline/`. The
//! status line reader loads that one file and never opens `SQLite`.

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write as _,
    path::Path,
    time::Duration,
};

use brgr_store::{BoardStore, SessionCounts};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{Paths, context_signal};

pub(crate) const HEARTBEAT: Duration = Duration::from_secs(30);
const STALE_AFTER: u64 = 120;
const REWRITE_AFTER: u64 = 20;
const SCHEMA: &str = "brgr-statusline/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Counts {
    pub running: u32,
    pub candidates: u32,
    pub failed: u32,
    pub asking: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Cache {
    schema: String,
    session: String,
    at: u64,
    running: u32,
    candidates: u32,
    failed: u32,
    asking: u32,
}

impl Counts {
    fn from_cache(cache: &Cache) -> Self {
        Self {
            running: cache.running,
            candidates: cache.candidates,
            failed: cache.failed,
            asking: cache.asking,
        }
    }

    fn from_row(row: &SessionCounts) -> Self {
        Self {
            running: row.running,
            candidates: row.candidates,
            failed: row.failed,
            asking: row.asking,
        }
    }
}

impl PartialEq<Counts> for Cache {
    fn eq(&self, other: &Counts) -> bool {
        self.running == other.running
            && self.candidates == other.candidates
            && self.failed == other.failed
            && self.asking == other.asking
    }
}

pub(crate) fn heartbeat() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(ms) = std::env::var("BRGR_TEST_WORKER_LINE_HEARTBEAT_MS")
        && let Ok(ms) = ms.parse::<u64>()
        && ms > 0
    {
        return Duration::from_millis(ms);
    }
    HEARTBEAT
}

fn rewrite_after() -> u64 {
    #[cfg(debug_assertions)]
    if std::env::var("BRGR_TEST_WORKER_LINE_HEARTBEAT_MS").is_ok() {
        return 0;
    }
    REWRITE_AFTER
}

fn statusline_dir(home: &Path) -> std::path::PathBuf {
    home.join("statusline")
}

/// Re-projects every bound session's counts. Never fails its caller.
pub(crate) fn refresh(paths: &Paths) {
    refresh_home(&paths.home);
}

pub(crate) fn refresh_home(home: &Path) {
    let dir = statusline_dir(home);
    if !dir.is_dir() {
        return;
    }
    let lock_path = dir.join(".lock");
    let Ok(lock) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    else {
        return;
    };
    if lock.lock().is_err() {
        return;
    }
    refresh_locked(home, &dir);
    drop(lock);
}

fn refresh_locked(home: &Path, dir: &Path) {
    let Ok(store) = BoardStore::open_existing(home.join("store")) else {
        return;
    };
    let Ok(rows) = store.session_counts() else {
        return;
    };
    let now = context_signal::now();
    let mut kept = HashSet::new();
    for row in rows {
        if !context_signal::valid_session(&row.session_id) {
            continue;
        }
        let counts = Counts::from_row(&row);
        let path = dir.join(format!("{}.json", row.session_id));
        let existing = load_cache(&path);
        let rewrite = existing
            .as_ref()
            .is_none_or(|cache| cache != &counts || now.saturating_sub(cache.at) > rewrite_after());
        if rewrite {
            let cache = Cache {
                schema: SCHEMA.to_owned(),
                session: row.session_id.clone(),
                at: now,
                running: counts.running,
                candidates: counts.candidates,
                failed: counts.failed,
                asking: counts.asking,
            };
            if write_cache(dir, &cache).is_err() {
                kept.insert(row.session_id);
                continue;
            }
        }
        kept.insert(row.session_id);
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !kept.contains(stem) {
            let _ = fs::remove_file(path);
        }
    }
}

fn load_cache(path: &Path) -> Option<Cache> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_cache(dir: &Path, cache: &Cache) -> std::io::Result<()> {
    let path = dir.join(format!("{}.json", cache.session));
    let mut temporary = NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(&mut temporary, cache).map_err(std::io::Error::other)?;
    temporary.write_all(b"\n")?;
    brgr_platform::fs::set_private_file_mode(temporary.as_file())?;
    temporary.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn read(paths: &Paths, session: &str, now: u64) -> Option<Counts> {
    if !context_signal::valid_session(session) {
        return None;
    }
    let path = statusline_dir(&paths.home).join(format!("{session}.json"));
    let cache: Cache = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    if cache.schema != SCHEMA || cache.session != session {
        return None;
    }
    if cache.running > 0 && now.saturating_sub(cache.at) > STALE_AFTER {
        return None;
    }
    Some(Counts::from_cache(&cache))
}

pub(crate) fn segment(counts: &Counts) -> Option<String> {
    if counts.running == 0 && counts.candidates == 0 && counts.failed == 0 && counts.asking == 0 {
        return None;
    }
    let mut text = format!(
        "brgr ⚙{} ✓{} ✗{}",
        counts.running, counts.candidates, counts.failed
    );
    if counts.asking > 0 {
        text.push_str(" ?");
        text.push_str(&counts.asking.to_string());
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brgr_protocol::{
        ArtifactContract, AttemptBudget, AttemptId, AttemptState, OwnerId, Route, SCHEMA_V1,
        TaskId, TaskSpec,
    };
    use brgr_store::Store;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn counts(running: u32, candidates: u32, failed: u32, asking: u32) -> Counts {
        Counts {
            running,
            candidates,
            failed,
            asking,
        }
    }

    fn home() -> (TempDir, Paths) {
        let temp = TempDir::new().unwrap();
        let paths = Paths::new(Some(temp.path().join("home"))).unwrap();
        (temp, paths)
    }

    fn enable(paths: &Paths) {
        let dir = statusline_dir(&paths.home);
        fs::create_dir_all(&dir).unwrap();
        brgr_platform::fs::set_private_dir(&dir).unwrap();
    }

    fn write_file(paths: &Paths, session: &str, at: u64, counts: &Counts) {
        enable(paths);
        let cache = Cache {
            schema: SCHEMA.to_owned(),
            session: session.to_owned(),
            at,
            running: counts.running,
            candidates: counts.candidates,
            failed: counts.failed,
            asking: counts.asking,
        };
        write_cache(&statusline_dir(&paths.home), &cache).unwrap();
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    fn spec(owner: &str, request: &str) -> TaskSpec {
        TaskSpec {
            schema: SCHEMA_V1.to_owned(),
            task_id: TaskId::new(),
            revision: 1,
            create_request_id: request.to_owned(),
            owner_id: OwnerId::new(owner).unwrap(),
            objective: "count".to_owned(),
            workspace: "/tmp/brgr-test".to_owned(),
            route: Route {
                harness_id: "local.fixture".to_owned(),
                requested_model: None,
                requested_effort: None,
            },
            required_capabilities: vec![],
            artifact_contract: ArtifactContract {
                media_type: "text/plain".to_owned(),
                max_bytes: 1_024,
            },
            acceptance_criteria: vec!["done".to_owned()],
            budget: AttemptBudget {
                deadline_seconds: 30,
                max_attempts: 2,
            },
            instructions: brgr_protocol::TaskInstructions::default(),
            evidence: brgr_protocol::EvidenceSpec::default(),
            max_concurrent_children: None,
            permission: None,
        }
    }

    fn running_task(store: &mut Store, owner: &str, request: &str, session: &str) {
        let task = spec(owner, request);
        store.record_task(&task, request).unwrap();
        store.bind_owner(&task.owner_id, session, 1).unwrap();
        let attempt = AttemptId::new();
        store
            .claim_attempt(task.task_id, task.revision, attempt)
            .unwrap();
        store
            .set_attempt_state(attempt, AttemptState::Starting)
            .unwrap();
        store
            .set_attempt_state(attempt, AttemptState::Running)
            .unwrap();
    }

    #[test]
    fn the_segment_shows_three_counts_and_the_question_mark_only_when_someone_asks() {
        assert_eq!(
            segment(&counts(3, 1, 0, 1)).as_deref(),
            Some("brgr ⚙3 ✓1 ✗0 ?1")
        );
        assert_eq!(
            segment(&counts(3, 1, 0, 0)).as_deref(),
            Some("brgr ⚙3 ✓1 ✗0")
        );
    }

    #[test]
    fn all_zero_counts_draw_nothing() {
        assert_eq!(segment(&counts(0, 0, 0, 0)), None);
    }

    #[test]
    fn a_missing_malformed_or_other_sessions_cache_draws_nothing() {
        let (_temp, paths) = home();
        let now = now();
        assert_eq!(read(&paths, "session-a", now), None);
        enable(&paths);
        fs::write(
            statusline_dir(&paths.home).join("session-a.json"),
            "not json",
        )
        .unwrap();
        assert_eq!(read(&paths, "session-a", now), None);
        write_file(&paths, "session-a", now, &counts(1, 0, 0, 0));
        let mut cache: serde_json::Value = serde_json::from_slice(
            &fs::read(statusline_dir(&paths.home).join("session-a.json")).unwrap(),
        )
        .unwrap();
        cache["schema"] = serde_json::json!("other");
        fs::write(
            statusline_dir(&paths.home).join("session-a.json"),
            cache.to_string(),
        )
        .unwrap();
        assert_eq!(read(&paths, "session-a", now), None);
        write_file(&paths, "session-b", now, &counts(1, 0, 0, 0));
        assert_eq!(read(&paths, "session-a", now), None);
    }

    #[test]
    fn a_cache_with_running_workers_older_than_the_heartbeat_limit_draws_nothing() {
        let (_temp, paths) = home();
        let now = 1_000;
        write_file(
            &paths,
            "session-a",
            now - STALE_AFTER - 1,
            &counts(1, 0, 0, 0),
        );
        assert_eq!(read(&paths, "session-a", now), None);
    }

    #[test]
    fn a_cache_with_only_results_waiting_has_no_age_limit() {
        let (_temp, paths) = home();
        let now = 1_000_000;
        write_file(&paths, "session-a", 1, &counts(0, 1, 0, 0));
        assert_eq!(read(&paths, "session-a", now), Some(counts(0, 1, 0, 0)));
    }

    #[test]
    fn refresh_does_nothing_when_the_statusline_directory_is_absent() {
        let (_temp, paths) = home();
        let mut store = Store::open(&paths.store).unwrap();
        running_task(&mut store, "codex:a", "a", "session-a");
        refresh(&paths);
        assert!(!statusline_dir(&paths.home).exists());
    }

    #[test]
    fn refresh_removes_the_file_of_a_session_with_nothing_left() {
        let (_temp, paths) = home();
        enable(&paths);
        write_file(&paths, "session-a", now(), &counts(1, 0, 0, 0));
        Store::open(&paths.store).unwrap();
        refresh(&paths);
        assert!(!statusline_dir(&paths.home).join("session-a.json").exists());
    }

    #[test]
    fn refresh_skips_a_session_id_that_is_not_a_file_name() {
        let (_temp, paths) = home();
        enable(&paths);
        let mut store = Store::open(&paths.store).unwrap();
        running_task(&mut store, "worker:attempt", "child", "worker:attempt");
        running_task(&mut store, "codex:a", "a", "session-a");
        refresh(&paths);
        assert!(
            !statusline_dir(&paths.home)
                .join("worker:attempt.json")
                .exists()
        );
        assert!(statusline_dir(&paths.home).join("session-a.json").exists());
    }

    #[test]
    fn refresh_rewrites_an_unchanged_file_only_after_the_rewrite_interval() {
        let (_temp, paths) = home();
        enable(&paths);
        let mut store = Store::open(&paths.store).unwrap();
        running_task(&mut store, "codex:a", "a", "session-a");
        let path = statusline_dir(&paths.home).join("session-a.json");
        write_file(&paths, "session-a", now(), &counts(1, 0, 0, 0));
        let before: Cache = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        refresh(&paths);
        let unchanged: Cache = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(unchanged.at, before.at);
        write_file(
            &paths,
            "session-a",
            now().saturating_sub(REWRITE_AFTER + 1),
            &counts(1, 0, 0, 0),
        );
        refresh(&paths);
        let rewritten: Cache = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(rewritten.at > before.at.saturating_sub(REWRITE_AFTER + 1));
        assert!(now().saturating_sub(rewritten.at) <= 2);
    }

    #[test]
    fn refresh_leaves_the_files_alone_when_the_store_cannot_be_read() {
        let (_temp, paths) = home();
        enable(&paths);
        write_file(&paths, "session-a", now(), &counts(1, 0, 0, 0));
        fs::create_dir_all(&paths.store).unwrap();
        fs::write(paths.store.join("brgr.sqlite3"), "not a database").unwrap();
        refresh(&paths);
        assert!(statusline_dir(&paths.home).join("session-a.json").exists());
    }
}
