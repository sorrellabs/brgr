//! Writer call sites for `$BRGR_HOME/statusline/<session>.json`.
//!
//! The status line reader never opens `SQLite`; these tests pin that a public
//! command, a supervisor, and a Codex hook re-project the cache.

mod common;

use common::{
    add_fixture, brgr_command, brgr_fixture, json_output, run, wait_until, write_fast_script,
};

use std::{
    fs,
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    thread,
};

use serde_json::Value;
use tempfile::TempDir;

const GATE_FIXTURE: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) echo 'fixture v-gate'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
if [ -n "$prompt_file" ] && /usr/bin/grep -q GATE "$prompt_file"; then
  : > "$BRGR_HOME/worker-ready"
  n=0
  while [ ! -f "$BRGR_HOME/worker-release" ] && [ "$n" -lt 6000 ]; do
    /bin/sleep 0.05
    n=$((n + 1))
  done
fi
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

const FAIL_FIXTURE: &str = r#"#!/bin/sh
case "$1" in
  --version) echo 'fixture v-failed-line'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
for item in "$@"; do case "$item" in @*) prompt=${item#@};; esac; done
if /usr/bin/grep -q BRGR_FIXTURE_OK "$prompt"; then
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
  printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
else
  exit 7
fi
"#;

const ASKING_FIXTURE: &str = r#"#!/bin/sh
set -u
case "${1:-}" in
  --version) echo 'fixture v-asking-line'; exit 0;;
  --help)
    printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'
    exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
answer=NO_WORKER_IDENTITY
if test -n "${BRGR_PARENT_ATTEMPT_ID:-}"; then
  if /usr/bin/grep -q '^ASK' "$prompt_file"; then
    "$BRGR_BIN" --json message send "$BRGR_PARENT_TASK_ID" --to owner --kind question --body 'Which token?' >/dev/null
    reply=$("$BRGR_BIN" --json message wait "$BRGR_PARENT_TASK_ID" --for worker --timeout-seconds 20)
    reply_id=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"message_id":"\([^"]*\)".*/\1/p')
    "$BRGR_BIN" --json message ack "$BRGR_PARENT_TASK_ID" "$reply_id" --for worker >/dev/null
    answer=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"body":"\([^"]*\)".*/\1/p')
  fi
fi
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"%s"}]}}\n' "$answer"
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

fn enable(home: &Path) {
    let dir = home.join("statusline");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
}

fn spec(owner: &str, request: &str) -> brgr_protocol::TaskSpec {
    brgr_protocol::TaskSpec {
        schema: brgr_protocol::SCHEMA_V1.to_owned(),
        task_id: brgr_protocol::TaskId::new(),
        revision: 1,
        create_request_id: request.to_owned(),
        owner_id: brgr_protocol::OwnerId::new(owner).unwrap(),
        objective: "count".to_owned(),
        workspace: "/tmp/brgr-test".to_owned(),
        route: brgr_protocol::Route {
            harness_id: "local.fixture".to_owned(),
            requested_model: None,
            requested_effort: None,
        },
        required_capabilities: vec![],
        artifact_contract: brgr_protocol::ArtifactContract {
            media_type: "text/plain".to_owned(),
            max_bytes: 1_024,
        },
        acceptance_criteria: vec!["done".to_owned()],
        budget: brgr_protocol::AttemptBudget {
            deadline_seconds: 30,
            max_attempts: 2,
        },
        instructions: brgr_protocol::TaskInstructions::default(),
        evidence: brgr_protocol::EvidenceSpec::default(),
        max_concurrent_children: None,
        permission: None,
    }
}

fn seed_failed(home: &Path, owner: &str, request: &str, session: &str) {
    let mut store = brgr_store::Store::open(home.join("store")).unwrap();
    let task = spec(owner, request);
    store.record_task(&task, request).unwrap();
    store.bind_owner(&task.owner_id, session, 1).unwrap();
    let attempt = brgr_protocol::AttemptId::new();
    store
        .claim_attempt(task.task_id, task.revision, attempt)
        .unwrap();
    let envelope = brgr_protocol::ResultEnvelope {
        schema: brgr_protocol::SCHEMA_V1.to_owned(),
        task_id: task.task_id,
        revision: task.revision,
        attempt_id: attempt,
        result_id: brgr_protocol::ResultId::new(),
        outcome: brgr_protocol::TerminalOutcome::Failed,
        artifacts: vec![],
        error: Some("fixture".to_owned()),
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec![],
        slept_seconds: None,
        harness_session_id: None,
    };
    store
        .commit_terminal_result(&task.owner_id, &envelope)
        .unwrap();
}

fn cache(home: &Path, session: &str) -> Value {
    serde_json::from_slice(
        &fs::read(home.join("statusline").join(format!("{session}.json"))).unwrap(),
    )
    .unwrap()
}

fn cache_exists(home: &Path, session: &str) -> bool {
    home.join("statusline")
        .join(format!("{session}.json"))
        .is_file()
}

fn counts(home: &Path, session: &str) -> Option<(u32, u32, u32, u32)> {
    if !cache_exists(home, session) {
        return None;
    }
    let value = cache(home, session);
    Some((
        u32::try_from(value["running"].as_u64()?).ok()?,
        u32::try_from(value["candidates"].as_u64()?).ok()?,
        u32::try_from(value["failed"].as_u64()?).ok()?,
        u32::try_from(value["asking"].as_u64()?).ok()?,
    ))
}

fn public_status(home: &Path) -> std::process::Output {
    brgr_command()
        .arg("--home")
        .arg(home)
        .args(["--json", "status"])
        .output()
        .unwrap()
}

fn json(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Value {
    json_output(&run(home, &[&["--json"][..], args].concat(), envs))
}

fn owner(id: &str, session: &str) -> Vec<(&'static str, String)> {
    vec![
        ("BRGR_OWNER_ID", id.to_owned()),
        ("BRGR_SESSION_ID", session.to_owned()),
    ]
}

fn env_pairs<'a>(vars: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    vars.iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect()
}

fn fixture_home() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    fs::create_dir_all(&workspace).unwrap();
    add_fixture(&home, &brgr_fixture(), &temp.path().join("scratch"));
    (temp, home, workspace)
}

fn custom_home(body: &str) -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr");
    let workspace = temp.path().join("work");
    let executable = temp.path().join("fixture");
    fs::create_dir_all(&workspace).unwrap();
    write_fast_script(&executable, body);
    add_fixture(&home, &executable, &temp.path().join("scratch"));
    (temp, home, workspace)
}

#[test]
fn a_public_command_writes_the_session_cache_when_the_statusline_directory_exists() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    seed_failed(&home, "codex:a", "a", "session-a");
    enable(&home);
    let output = public_status(&home);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = cache(&home, "session-a");
    assert_eq!(value["schema"], "brgr-statusline/v1");
    assert_eq!(value["session"], "session-a");
    assert_eq!(value["running"], 0);
    assert_eq!(value["candidates"], 0);
    assert_eq!(value["failed"], 1);
    assert_eq!(value["asking"], 0);
}

#[test]
fn a_public_command_does_not_create_the_statusline_directory() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    seed_failed(&home, "codex:a", "a", "session-a");
    let output = public_status(&home);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.join("statusline").exists());
}

#[test]
fn a_hook_writes_the_session_cache() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    seed_failed(&home, "codex:a", "a", "session-a");
    enable(&home);
    let mut child = brgr_command()
        .arg("--home")
        .arg(&home)
        .args(["__hook", "stop"])
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_PANE_ID")
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"session-a"}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(cache(&home, "session-a")["failed"], 1);
}

#[test]
fn context_does_not_write_the_cache() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    seed_failed(&home, "codex:a", "a", "session-a");
    enable(&home);
    let mut child = brgr_command()
        .arg("--home")
        .arg(&home)
        .args(["__context", "--workers"])
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(include_bytes!(
            "../../../testdata/fixtures/statusline/claude-2.1.293.json"
        ))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_dir(home.join("statusline"))
            .unwrap()
            .flatten()
            .all(|entry| entry.path().extension().is_none_or(|ext| ext != "json"))
    );
}

#[test]
fn starting_a_task_writes_the_owners_cache_and_a_candidate_moves_it_from_running_to_waiting() {
    let (_temp, home, workspace) = custom_home(GATE_FIXTURE);
    enable(&home);
    let vars = owner("codex:line-run", "session-run");
    let envs = env_pairs(&vars);
    let launched = json(
        &home,
        &["run", "GATE", "--workspace", workspace.to_str().unwrap()],
        &envs,
    );
    let task = launched["task_id"].as_str().unwrap().to_owned();
    assert!(
        wait_until(|| counts(&home, "session-run") == Some((1, 0, 0, 0))),
        "running cache never appeared: {:?}",
        counts(&home, "session-run")
    );
    fs::write(home.join("worker-release"), "").unwrap();
    assert!(
        wait_until(|| {
            let _ = run(&home, &["--json", "status", &task], &envs);
            counts(&home, "session-run") == Some((0, 1, 0, 0))
        }),
        "candidate cache never appeared: {:?}",
        counts(&home, "session-run")
    );
}

#[test]
fn accept_clears_the_waiting_result_and_removes_the_cache_file() {
    let (_temp, home, workspace) = fixture_home();
    enable(&home);
    let vars = owner("codex:line-accept", "session-accept");
    let envs = env_pairs(&vars);
    let launched = json(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &envs,
    );
    let task = launched["task_id"].as_str().unwrap();
    assert_eq!(counts(&home, "session-accept"), Some((0, 1, 0, 0)));
    json(&home, &["accept", task, "--reason", "verified"], &envs);
    assert!(!cache_exists(&home, "session-accept"));
}

#[test]
fn a_failed_run_counts_until_result_ack() {
    let (_temp, home, workspace) = custom_home(FAIL_FIXTURE);
    enable(&home);
    let vars = owner("codex:line-fail", "session-fail");
    let envs = env_pairs(&vars);
    let launched = json(
        &home,
        &[
            "run",
            "FAIL",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &envs,
    );
    assert_eq!(launched["outcome"], "failed");
    let task = launched["task_id"].as_str().unwrap();
    assert_eq!(counts(&home, "session-fail"), Some((0, 0, 1, 0)));
    json(&home, &["result", task, "--ack"], &envs);
    assert!(!cache_exists(&home, "session-fail"));
}

#[test]
fn a_worker_question_counts_until_the_owner_replies() {
    let (_temp, home, workspace) = custom_home(ASKING_FIXTURE);
    enable(&home);
    let vars = owner("codex:line-ask", "session-ask");
    let envs = env_pairs(&vars);
    let launched = json(
        &home,
        &["run", "ASK", "--workspace", workspace.to_str().unwrap()],
        &envs,
    );
    let task = launched["task_id"].as_str().unwrap().to_owned();
    assert!(
        wait_until(|| counts(&home, "session-ask") == Some((1, 0, 0, 1))),
        "asking cache never appeared: {:?}",
        counts(&home, "session-ask")
    );
    let question = json(
        &home,
        &[
            "message",
            "wait",
            &task,
            "--for",
            "owner",
            "--timeout-seconds",
            "60",
        ],
        &envs,
    );
    let question_id = question["message_id"].as_str().unwrap();
    json(
        &home,
        &[
            "message",
            "send",
            &task,
            "--to",
            "worker",
            "--kind",
            "reply",
            "--reply-to",
            question_id,
            "--body",
            "token-42",
        ],
        &envs,
    );
    assert!(
        wait_until(|| counts(&home, "session-ask").is_none_or(|row| row.3 == 0)),
        "asking count stayed set: {:?}",
        counts(&home, "session-ask")
    );
}

#[test]
fn two_owners_in_two_sessions_each_see_only_their_own_workers() {
    let (_temp, home, workspace) = fixture_home();
    enable(&home);
    let one = owner("codex:line-one", "session-one");
    let two = owner("codex:line-two", "session-two");
    json(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &env_pairs(&one),
    );
    json(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &env_pairs(&two),
    );
    assert_eq!(counts(&home, "session-one"), Some((0, 1, 0, 0)));
    assert_eq!(counts(&home, "session-two"), Some((0, 1, 0, 0)));
}

#[test]
fn moving_the_head_moves_the_counts_to_the_new_session() {
    let (_temp, home, workspace) = fixture_home();
    enable(&home);
    let outgoing = owner("codex:line-head", "session-old");
    let launched = json(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &env_pairs(&outgoing),
    );
    let task = launched["task_id"].as_str().unwrap();
    assert_eq!(counts(&home, "session-old"), Some((0, 1, 0, 0)));
    let incoming = owner("codex:line-head", "session-new");
    json(
        &home,
        &[
            "head",
            "claim",
            task,
            "--abandoned",
            "--goal",
            "take the line",
        ],
        &env_pairs(&incoming),
    );
    assert!(!cache_exists(&home, "session-old"));
    assert_eq!(counts(&home, "session-new"), Some((0, 1, 0, 0)));
}

#[cfg(debug_assertions)]
#[test]
fn a_supervisor_heartbeat_keeps_a_running_cache_fresh() {
    let (_temp, home, workspace) = custom_home(GATE_FIXTURE);
    enable(&home);
    let vars = owner("codex:line-beat", "session-beat");
    let mut envs = env_pairs(&vars);
    envs.push(("BRGR_TEST_WORKER_LINE_HEARTBEAT_MS", "50"));
    json(
        &home,
        &["run", "GATE", "--workspace", workspace.to_str().unwrap()],
        &envs,
    );
    assert!(
        wait_until(|| counts(&home, "session-beat") == Some((1, 0, 0, 0))),
        "running cache never appeared"
    );
    let first = cache(&home, "session-beat")["at"].as_u64().unwrap();
    assert!(
        wait_until(|| {
            cache_exists(&home, "session-beat")
                && cache(&home, "session-beat")["at"].as_u64().unwrap() > first
                && cache(&home, "session-beat")["running"] == 1
        }),
        "heartbeat never rewrote at={first}: {:?}",
        cache(&home, "session-beat")
    );
    fs::write(home.join("worker-release"), "").unwrap();
}

#[cfg(debug_assertions)]
#[test]
fn a_run_lost_by_recovery_leaves_running_and_becomes_failed() {
    let (_temp, home, workspace) = fixture_home();
    enable(&home);
    let vars = owner("codex:line-lost", "session-lost");
    let mut envs = env_pairs(&vars);
    envs.push(("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM", "1"));
    let launched = json(
        &home,
        &[
            "run",
            "no model effect",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        &envs,
    );
    let task = launched["task_id"].as_str().unwrap().to_owned();
    assert_eq!(counts(&home, "session-lost"), Some((1, 0, 0, 0)));
    let owner_envs = env_pairs(&vars);
    assert!(
        wait_until(|| {
            let _ = run(&home, &["--json", "status", &task], &owner_envs);
            counts(&home, "session-lost") == Some((0, 0, 1, 0))
        }),
        "lost run never became failed: {:?}",
        counts(&home, "session-lost")
    );
}

#[test]
fn nothing_is_written_under_statusline_when_workers_are_not_enabled() {
    let (_temp, home, workspace) = fixture_home();
    let vars = owner("codex:line-off", "session-off");
    json(
        &home,
        &[
            "run",
            "BRGR_FIXTURE_OK",
            "--workspace",
            workspace.to_str().unwrap(),
            "--foreground",
        ],
        &env_pairs(&vars),
    );
    assert!(!home.join("statusline").exists());
}

#[test]
fn concurrent_refreshes_end_with_the_latest_counts() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    seed_failed(&home, "codex:line-race", "one", "session-race");
    enable(&home);
    let refresh = || {
        let output = public_status(&home);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(refresh);
        }
    });
    assert_eq!(counts(&home, "session-race"), Some((0, 0, 1, 0)));
    seed_failed(&home, "codex:line-race", "two", "session-race");
    thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(refresh);
        }
    });
    assert_eq!(counts(&home, "session-race"), Some((0, 0, 2, 0)));
}
