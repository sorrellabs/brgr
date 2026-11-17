//! Read-only Herdr board projection. Never returns objective or artifact bytes.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::StoreError;
use brgr_protocol::{
    AttemptState, DecisionVerdict, OwnerId, ResultId, SCHEMA_V1, TaskId, TerminalOutcome,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, Params, Transaction, params};

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

const BOARD_SELECT: &str = "WITH latest_attempts AS (
             SELECT a.task_id, a.revision, a.state
             FROM attempts a
             WHERE a.rowid = (
                 SELECT MAX(candidate.rowid) FROM attempts candidate
                 WHERE candidate.task_id = a.task_id AND candidate.revision = a.revision
             )
         ),
         latest_results AS (
             SELECT r.result_id, r.task_id, r.revision, r.envelope_json
             FROM results r
             WHERE r.rowid = (
                 SELECT MAX(candidate.rowid) FROM results candidate
                 WHERE candidate.task_id = r.task_id AND candidate.revision = r.revision
             )
         )
         SELECT CAST(t.task_id AS TEXT), CAST(t.revision AS TEXT),
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.schema') = 'text' THEN CAST(json_extract(t.spec_json, '$.schema') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.task_id') = 'text' THEN CAST(json_extract(t.spec_json, '$.task_id') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.revision') = 'integer' THEN CAST(json_extract(t.spec_json, '$.revision') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.workspace') = 'text' THEN CAST(json_extract(t.spec_json, '$.workspace') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.route.harness_id') = 'text' THEN CAST(json_extract(t.spec_json, '$.route.harness_id') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.route.requested_model') = 'text' THEN CAST(json_extract(t.spec_json, '$.route.requested_model') AS TEXT) END END,
                CASE WHEN json_valid(t.spec_json) THEN CASE WHEN json_type(t.spec_json, '$.route.requested_effort') = 'text' THEN CAST(json_extract(t.spec_json, '$.route.requested_effort') AS TEXT) END END,
                CAST(a.state AS TEXT),
                CAST(r.result_id AS TEXT),
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.schema') = 'text' THEN CAST(json_extract(r.envelope_json, '$.schema') AS TEXT) END END,
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.task_id') = 'text' THEN CAST(json_extract(r.envelope_json, '$.task_id') AS TEXT) END END,
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.revision') = 'integer' THEN CAST(json_extract(r.envelope_json, '$.revision') AS TEXT) END END,
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.result_id') = 'text' THEN CAST(json_extract(r.envelope_json, '$.result_id') AS TEXT) END END,
                CASE WHEN json_valid(r.envelope_json) THEN CASE WHEN json_type(r.envelope_json, '$.outcome') = 'text' THEN CAST(json_extract(r.envelope_json, '$.outcome') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.schema') = 'text' THEN CAST(json_extract(d.decision_json, '$.schema') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.task_id') = 'text' THEN CAST(json_extract(d.decision_json, '$.task_id') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.revision') = 'integer' THEN CAST(json_extract(d.decision_json, '$.revision') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.result_id') = 'text' THEN CAST(json_extract(d.decision_json, '$.result_id') AS TEXT) END END,
                CASE WHEN json_valid(d.decision_json) THEN CASE WHEN json_type(d.decision_json, '$.verdict') = 'text' THEN CAST(json_extract(d.decision_json, '$.verdict') AS TEXT) END END
         FROM tasks t
         JOIN (
             SELECT task_id, MAX(revision) AS revision FROM tasks GROUP BY task_id
         ) latest
           ON latest.task_id = t.task_id AND latest.revision = t.revision
         LEFT JOIN latest_attempts a
           ON a.task_id = t.task_id AND a.revision = t.revision
         LEFT JOIN latest_results r
           ON r.task_id = t.task_id AND r.revision = t.revision
         LEFT JOIN decisions d ON d.result_id = r.result_id";

const BOARD_ORDER: &str = "
         ORDER BY t.rowid DESC
         LIMIT 100";

const BOARD_SESSION_FILTER: &str = "
         WHERE EXISTS (SELECT 1 FROM owner_bindings b WHERE b.owner_id = t.owner_id AND b.session_id = ?1) AND (?2 IS NULL OR t.owner_id = ?2)";

/// Read-only Herdr board row. Never includes the task objective or artifact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoardTaskRow {
    pub task_id: TaskId,
    pub revision: u32,
    pub workspace: String,
    /// The primary checkout brgr recorded for a task worktree, if any. The
    /// workspace records the worktree brgr created, not the repository it came
    /// from.
    pub checkout: Option<String>,
    pub harness_id: String,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub attempt_state: AttemptState,
    pub result_outcome: Option<TerminalOutcome>,
    pub decision_verdict: Option<DecisionVerdict>,
}

/// Per-session worker counts for a bound owner's status line. Never includes
/// task ids, owner ids, objectives, or message text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCounts {
    pub session_id: String,
    pub running: u32,
    pub candidates: u32,
    pub failed: u32,
    pub asking: u32,
}

/// Existing-database reader for the Herdr board. Never applies schema and never
/// opens artifact storage.
pub struct BoardStore {
    connection: Option<Connection>,
}

struct ProjectedBoardRow {
    stored_task_id: String,
    stored_revision: String,
    spec_schema: Option<String>,
    spec_task_id: Option<String>,
    spec_revision: Option<String>,
    workspace: Option<String>,
    harness_id: Option<String>,
    requested_model: Option<String>,
    requested_effort: Option<String>,
    state: Option<String>,
    stored_result_id: Option<String>,
    result_schema: Option<String>,
    result_task_id: Option<String>,
    result_revision: Option<String>,
    result_id: Option<String>,
    outcome: Option<String>,
    decision_schema: Option<String>,
    decision_task_id: Option<String>,
    decision_revision: Option<String>,
    decision_result_id: Option<String>,
    verdict: Option<String>,
}

impl BoardStore {
    /// Opens a query-only connection to an already initialized store database.
    ///
    /// A missing database is treated as an empty board. An existing file is
    /// opened read-only with a one-second busy timeout and never receives
    /// schema or artifact initialization.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing database cannot be opened.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let database_path = PathBuf::from(root.as_ref()).join("brgr.sqlite3");
        if !database_path.is_file() {
            return Ok(Self { connection: None });
        }
        let connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "query_only", "ON")?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    /// Lists newest task revisions for the read-only Herdr board.
    ///
    /// One deferred snapshot joins the latest revision, its latest attempt,
    /// that revision's latest terminal result, and the decision for that
    /// result. Malformed specs and invalid attempt states are skipped. A
    /// decision is shown only when that same result envelope is valid. Rows
    /// never include objective text or artifact bytes. SQL projects only the
    /// JSON fields needed by the board into Rust values.
    ///
    /// # Errors
    ///
    /// Returns an error for a database failure.
    pub fn rows(&self, limit: usize) -> Result<Vec<BoardTaskRow>, StoreError> {
        self.load_rows(limit, None)
    }

    /// Lists newest task revisions whose owner is bound to `session_id`.
    ///
    /// The optional owner narrows an explicit `BRGR_OWNER_ID` without allowing
    /// unrelated owners to consume the result limit. This query names
    /// `owner_bindings`; [`rows`] does not, so an older database without that
    /// table can still list every owner.
    ///
    /// # Errors
    ///
    /// Returns an error for a database failure.
    pub fn rows_for_session(
        &self,
        session_id: &str,
        owner_id: Option<&OwnerId>,
        limit: usize,
    ) -> Result<Vec<BoardTaskRow>, StoreError> {
        self.load_rows(limit, Some((session_id, owner_id)))
    }

    fn load_rows(
        &self,
        limit: usize,
        session: Option<(&str, Option<&OwnerId>)>,
    ) -> Result<Vec<BoardTaskRow>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let bounded = i64::try_from(limit.min(100)).map_err(|_| StoreError::InvalidTaskLimit)?;
        let Some(connection) = self.connection.as_ref() else {
            return Ok(Vec::new());
        };
        load_board_rows(connection, bounded, session)
    }

    /// One row per bound session that has at least one non-zero count.
    ///
    /// Classification of each latest revision matches [`crate::Store::revision_settlement`].
    /// `asking` is the subset of `running` whose active attempt has an unanswered
    /// `worker_to_owner` question. Malformed result envelopes are skipped.
    ///
    /// # Errors
    ///
    /// Returns an error for a database failure.
    pub fn session_counts(&self) -> Result<Vec<SessionCounts>, StoreError> {
        let Some(connection) = self.connection.as_ref() else {
            return Ok(Vec::new());
        };
        load_session_counts(connection)
    }
}

fn load_board_rows(
    connection: &Connection,
    limit: i64,
    session: Option<(&str, Option<&OwnerId>)>,
) -> Result<Vec<BoardTaskRow>, StoreError> {
    let transaction = connection.unchecked_transaction()?;
    let rows = load_board_rows_in_transaction(&transaction, limit, session)?;
    transaction.commit()?;
    Ok(rows)
}

fn all_owners_sql() -> String {
    format!("{BOARD_SELECT}{BOARD_ORDER}")
}

fn session_sql() -> String {
    format!("{BOARD_SELECT}{BOARD_SESSION_FILTER}{BOARD_ORDER}")
}

fn load_board_rows_in_transaction(
    transaction: &Transaction<'_>,
    limit: i64,
    session: Option<(&str, Option<&OwnerId>)>,
) -> Result<Vec<BoardTaskRow>, StoreError> {
    match session {
        None => collect_board_rows(transaction, &all_owners_sql(), [], limit),
        Some((session_id, owner_id)) => collect_board_rows(
            transaction,
            &session_sql(),
            params![session_id, owner_id.map(OwnerId::as_str)],
            limit,
        ),
    }
}

fn collect_board_rows(
    transaction: &Transaction<'_>,
    sql: &str,
    params: impl Params,
    limit: i64,
) -> Result<Vec<BoardTaskRow>, StoreError> {
    let mut statement = transaction.prepare(sql)?;
    let mapped = statement.query_map(params, map_projected_row)?;
    let mut rows = Vec::new();
    for entry in mapped {
        let Some(row) = parse_board_row(entry?) else {
            continue;
        };
        rows.push(row);
        if i64::try_from(rows.len()).unwrap_or(i64::MAX) >= limit {
            break;
        }
    }
    attach_task_checkouts(transaction, &mut rows)?;
    Ok(rows)
}

/// Fills each row with the primary checkout recorded for its revision.
///
/// A store written before `task_checkouts` existed has no such table; the board
/// still lists every task and leaves the checkout unset.
fn attach_task_checkouts(
    transaction: &Transaction<'_>,
    rows: &mut [BoardTaskRow],
) -> Result<(), StoreError> {
    if rows.is_empty() || !table_exists(transaction, "task_checkouts")? {
        return Ok(());
    }
    {
        let mut statement = transaction.prepare(
            "SELECT primary_checkout FROM task_checkouts WHERE task_id = ?1 AND revision = ?2",
        )?;
        for row in rows {
            row.checkout = statement
                .query_row(params![row.task_id.to_string(), row.revision], |value| {
                    value.get::<_, String>(0)
                })
                .optional()?;
        }
    }
    Ok(())
}

fn table_exists(transaction: &Transaction<'_>, name: &str) -> Result<bool, StoreError> {
    let exists = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(exists == 1)
}

fn map_projected_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectedBoardRow> {
    Ok(ProjectedBoardRow {
        stored_task_id: row.get(0)?,
        stored_revision: row.get(1)?,
        spec_schema: row.get(2)?,
        spec_task_id: row.get(3)?,
        spec_revision: row.get(4)?,
        workspace: row.get(5)?,
        harness_id: row.get(6)?,
        requested_model: row.get(7)?,
        requested_effort: row.get(8)?,
        state: row.get(9)?,
        stored_result_id: row.get(10)?,
        result_schema: row.get(11)?,
        result_task_id: row.get(12)?,
        result_revision: row.get(13)?,
        result_id: row.get(14)?,
        outcome: row.get(15)?,
        decision_schema: row.get(16)?,
        decision_task_id: row.get(17)?,
        decision_revision: row.get(18)?,
        decision_result_id: row.get(19)?,
        verdict: row.get(20)?,
    })
}

fn load_session_counts(connection: &Connection) -> Result<Vec<SessionCounts>, StoreError> {
    let transaction = connection.unchecked_transaction()?;
    let rows = load_session_counts_in_transaction(&transaction)?;
    transaction.commit()?;
    Ok(rows)
}

fn load_session_counts_in_transaction(
    transaction: &Transaction<'_>,
) -> Result<Vec<SessionCounts>, StoreError> {
    let mut statement = transaction.prepare(
        "WITH latest AS (
             SELECT task_id, MAX(revision) AS revision FROM tasks GROUP BY task_id
         ),
         scoped AS (
             SELECT b.session_id, t.task_id, t.revision
             FROM tasks t
             JOIN latest l ON l.task_id = t.task_id AND l.revision = t.revision
             JOIN owner_bindings b ON b.owner_id = t.owner_id
         ),
         facts AS (
             SELECT s.session_id, s.task_id,
               (SELECT a.attempt_id FROM attempts a
                 WHERE a.task_id = s.task_id AND a.revision = s.revision
                   AND a.state <> 'terminal'
                 LIMIT 1) AS active_attempt,
               (SELECT r.result_id FROM results r
                 WHERE r.task_id = s.task_id AND r.revision = s.revision
                 ORDER BY r.rowid DESC LIMIT 1) AS result_id
             FROM scoped s
         ),
         classified AS (
             SELECT f.session_id, f.task_id, f.active_attempt, f.result_id,
               r.attempt_id AS result_attempt,
               CASE WHEN json_valid(r.envelope_json)
                         AND json_type(r.envelope_json, '$.outcome') = 'text'
                    THEN CAST(json_extract(r.envelope_json, '$.outcome') AS TEXT)
               END AS outcome
             FROM facts f LEFT JOIN results r ON r.result_id = f.result_id
         )
         SELECT session_id,
           SUM(is_running), SUM(is_candidate), SUM(is_failed), SUM(is_asking)
         FROM (
           SELECT c.session_id,
             CASE WHEN c.active_attempt IS NOT NULL OR c.result_id IS NULL
                       OR (c.outcome <> 'candidate' AND EXISTS (
                             SELECT 1 FROM pre_spawn_retry_grants g
                             WHERE g.attempt_id = c.result_attempt))
                  THEN 1 ELSE 0 END AS is_running,
             CASE WHEN c.active_attempt IS NULL AND c.outcome = 'candidate'
                       AND NOT EXISTS (
                             SELECT 1 FROM decisions d WHERE d.result_id = c.result_id)
                  THEN 1 ELSE 0 END AS is_candidate,
             CASE WHEN c.active_attempt IS NULL
                       AND c.outcome IN ('failed', 'lost')
                       AND NOT EXISTS (
                             SELECT 1 FROM pre_spawn_retry_grants g
                             WHERE g.attempt_id = c.result_attempt)
                       AND NOT EXISTS (
                             SELECT 1 FROM inbox_items i
                             WHERE i.result_id = c.result_id AND i.acknowledged = 1)
                  THEN 1 ELSE 0 END AS is_failed,
             CASE WHEN c.active_attempt IS NOT NULL AND EXISTS (
                         SELECT 1 FROM task_messages q
                         WHERE q.task_id = c.task_id
                           AND q.attempt_id = c.active_attempt
                           AND q.direction = 'worker_to_owner'
                           AND q.kind = 'question'
                           AND NOT EXISTS (
                                 SELECT 1 FROM task_messages r2
                                 WHERE r2.in_reply_to = q.message_id AND r2.kind = 'reply')
                           AND NOT EXISTS (
                                 SELECT 1 FROM withdrawn_questions w
                                 WHERE w.message_id = q.message_id))
                  THEN 1 ELSE 0 END AS is_asking
           FROM classified c
         )
         GROUP BY session_id
         HAVING SUM(is_running) + SUM(is_candidate) + SUM(is_failed) > 0",
    )?;
    let mapped = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    mapped
        .map(|entry| {
            let (session_id, running, candidates, failed, asking) = entry?;
            Ok(SessionCounts {
                session_id,
                running: u32::try_from(running).map_err(|_| StoreError::NumericOverflow)?,
                candidates: u32::try_from(candidates).map_err(|_| StoreError::NumericOverflow)?,
                failed: u32::try_from(failed).map_err(|_| StoreError::NumericOverflow)?,
                asking: u32::try_from(asking).map_err(|_| StoreError::NumericOverflow)?,
            })
        })
        .collect()
}

fn parse_board_row(row: ProjectedBoardRow) -> Option<BoardTaskRow> {
    let task_id = row.stored_task_id.parse::<TaskId>().ok()?;
    let revision = row.stored_revision.parse::<u32>().ok()?;
    let workspace = row.workspace?;
    let harness_id = row.harness_id?;
    if row.spec_schema.as_deref() != Some(SCHEMA_V1)
        || row.spec_task_id.as_deref() != Some(row.stored_task_id.as_str())
        || row.spec_revision.as_deref() != Some(row.stored_revision.as_str())
        || harness_id.trim().is_empty()
        || workspace.trim().is_empty()
    {
        return None;
    }
    let attempt_state = match row.state.as_deref() {
        None => AttemptState::Queued,
        Some(value) => parse_attempt_state(value)?,
    };
    let result = row
        .stored_result_id
        .as_deref()
        .and_then(|id| id.parse::<ResultId>().ok())
        .and_then(|stored_result_id| {
            let envelope_result_id = row.result_id.as_deref()?.parse::<ResultId>().ok()?;
            let outcome = parse_terminal_outcome(row.outcome.as_deref()?)?;
            (row.result_schema.as_deref() == Some(SCHEMA_V1)
                && row.result_task_id.as_deref() == Some(row.stored_task_id.as_str())
                && row.result_revision.as_deref() == Some(row.stored_revision.as_str())
                && envelope_result_id == stored_result_id)
                .then_some((stored_result_id, outcome))
        });
    let decision_verdict = result.as_ref().and_then(|(result_id, _)| {
        let decision_result_id = row
            .decision_result_id
            .as_deref()?
            .parse::<ResultId>()
            .ok()?;
        let verdict = parse_decision_verdict(row.verdict.as_deref()?)?;
        (row.decision_schema.as_deref() == Some(SCHEMA_V1)
            && row.decision_task_id.as_deref() == Some(row.stored_task_id.as_str())
            && row.decision_revision.as_deref() == Some(row.stored_revision.as_str())
            && decision_result_id == *result_id)
            .then_some(verdict)
    });
    let result_outcome = result.map(|(_, outcome)| outcome);
    Some(BoardTaskRow {
        task_id,
        revision,
        workspace,
        checkout: None,
        harness_id,
        requested_model: row.requested_model,
        requested_effort: row.requested_effort,
        attempt_state,
        result_outcome,
        decision_verdict,
    })
}

fn parse_attempt_state(value: &str) -> Option<AttemptState> {
    match value {
        "queued" => Some(AttemptState::Queued),
        "starting" => Some(AttemptState::Starting),
        "running" => Some(AttemptState::Running),
        "blocked" => Some(AttemptState::Blocked),
        "collecting" => Some(AttemptState::Collecting),
        "cancel_requested" => Some(AttemptState::CancelRequested),
        "terminal" => Some(AttemptState::Terminal),
        _ => None,
    }
}

fn parse_terminal_outcome(value: &str) -> Option<TerminalOutcome> {
    match value {
        "candidate" => Some(TerminalOutcome::Candidate),
        "failed" => Some(TerminalOutcome::Failed),
        "cancelled" => Some(TerminalOutcome::Cancelled),
        "lost" => Some(TerminalOutcome::Lost),
        _ => None,
    }
}

fn parse_decision_verdict(value: &str) -> Option<DecisionVerdict> {
    match value {
        "accepted" => Some(DecisionVerdict::Accepted),
        "rejected" => Some(DecisionVerdict::Rejected),
        _ => None,
    }
}
