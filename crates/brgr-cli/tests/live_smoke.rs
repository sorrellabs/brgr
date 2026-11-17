//! The nightly live smoke stays runnable by hand, never reaches a fork, and
//! keeps harness screen text out of anything that leaves this machine.
//!
//! These check the parts that CI can check without a Herdr session, a self-hosted
//! runner or a logged-in harness: the script parses under `bash -n`, reports the
//! tier 1 harnesses and their cheapest models from `--check`, redacts a brgr
//! error that carries a screen, and the workflow can only be started by a
//! schedule, a manual dispatch or a release tag. A real round is the nightly
//! job's; `docs/guides/reference.md` says so.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above the crate")
        .to_path_buf()
}

fn script() -> PathBuf {
    repository_root().join("scripts/live-smoke.sh")
}

fn workflow() -> String {
    fs::read_to_string(repository_root().join(".github/workflows/live-smoke.yml"))
        .expect("the live smoke workflow exists")
}

fn script_source() -> String {
    fs::read_to_string(script()).expect("the live smoke script exists")
}

/// A bash run of the script with a clean environment, so nothing from the
/// machine running the tests changes what the script decides.
fn bash(arguments: &[&str]) -> std::process::Output {
    let mut command = Command::new("bash");
    command.args(arguments);
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text.starts_with("BRGR_") || text.starts_with("HERDR_") {
            command.env_remove(&name);
        }
    }
    command.env("BRGR_LIVE_SMOKE_SKIP_UPDATE", "1");
    command.output().expect("bash runs")
}

#[test]
fn the_live_smoke_script_parses_without_running_a_harness() {
    let output = bash(&["-n", script().to_str().unwrap()]);
    assert!(
        output.status.success(),
        "bash -n: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_names_every_tier_one_harness_and_its_cheapest_model() {
    let root = tempfile::tempdir().unwrap();
    let artifacts = root.path().join("artifacts");
    let output = bash(&[
        script().to_str().unwrap(),
        "--check",
        "--root",
        root.path().to_str().unwrap(),
        "--artifacts",
        artifacts.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "--check: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "local.claude-code",
        "model=haiku",
        "local.codex",
        "model=gpt-6-luna",
        "local.cursor-cli",
        "model=composer-2.5",
        "local.devin",
        "model=<devin default>",
    ] {
        assert!(
            plan.contains(expected),
            "the plan is missing {expected}: {plan}"
        );
    }
}

/// A brgr failure quotes the pane after `last screen:` and never says what a
/// worker was asked. Filing one of those would put a harness screen in a public
/// issue, so the script cuts it before it records a failure class.
#[test]
fn a_failure_class_is_cut_before_the_screen_and_the_home_path() {
    let source = script_source();
    for marker in [
        " last screen:",
        " last stderr:",
        " last error event:",
        " no brgr rule answers:",
    ] {
        assert!(source.contains(marker), "sanitize must cut at {marker}");
    }
    assert!(
        source.contains(r"s#/Users/[^ ]*#<path>#g"),
        "sanitize must mask a home path"
    );

    // The cut itself, run the way the script runs it.
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("sanitize.sh");
    let start = source
        .find("sanitize() {")
        .expect("the script defines sanitize");
    let end = source[start..]
        .find("\n}\n")
        .map(|offset| start + offset + 3)
        .expect("sanitize ends with a closing brace");
    fs::write(
        &script,
        format!(
            "set -euo pipefail\nHOME=/Users/example\n{}",
            &source[start..end]
        ),
    )
    .unwrap();
    let sample = "at 12:00 no brgr rule answers the screen: Allow this tool? \
                  last screen: Do you trust the files in /Users/example/project? \
                  last stderr: worker said something private";
    let output = bash(&[
        "-c",
        &format!(". {}; sanitize \"$1\"", script.display()),
        "--",
        sample,
    ]);
    assert!(
        output.status.success(),
        "sanitize: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cleaned = String::from_utf8_lossy(&output.stdout);
    assert!(
        !cleaned.contains("Do you trust"),
        "the screen was kept: {cleaned}"
    );
    assert!(
        !cleaned.contains("worker said"),
        "the worker text was kept: {cleaned}"
    );
    assert!(!cleaned.contains("/Users/example"), "{cleaned}");
    assert!(cleaned.contains("no brgr rule answers"), "{cleaned}");
}

#[test]
fn the_workflow_cannot_be_started_by_a_fork_pull_request() {
    let workflow = workflow();
    let triggers = workflow
        .split("\non:")
        .nth(1)
        .expect("the workflow declares its triggers")
        .split("\npermissions:")
        .next()
        .expect("a permissions block follows the triggers");
    for forbidden in ["pull_request", "pull_request_target"] {
        assert!(
            !triggers.contains(forbidden),
            "{forbidden} would run fork code on the self-hosted runner"
        );
    }
    for required in ["schedule", "workflow_dispatch", "tags:"] {
        assert!(triggers.contains(required), "the workflow lost {required}");
    }
}

#[test]
fn the_workflow_runs_on_the_labelled_self_hosted_machine() {
    let workflow = workflow();
    assert!(
        workflow.contains("runs-on: [self-hosted, macOS, brgr-live]"),
        "the live smoke must run on the brgr-live runner"
    );
}

/// The suite runs in a Herdr pane, and the pane's shell has to report the
/// suite's status back. Escaping `$?` one level too far made the pane write the
/// literal text `$?`, so every run looked like it passed. This reproduces the
/// exact string the script hands to `herdr pane run` and checks that a failing
/// child comes back as its own exit code.
#[test]
fn the_pane_run_reports_the_suites_own_exit_status() {
    let source = script_source();
    let line = source
        .lines()
        .find(|line| line.contains("printf '%s\\\\n' \\\"\\$?\\\""))
        .expect("delegate_into_herdr builds the status-writing command");
    assert!(
        !line.contains("\\\\\\$?"),
        "the status must be expanded by the pane, not escaped: {line}"
    );

    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("child.sh");
    fs::write(&child, "#!/usr/bin/env bash\nexit 7\n").unwrap();
    fs::set_permissions(&child, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let exit_file = root.path().join("exit");
    let log_file = root.path().join("log");

    // The same wrapping the script builds around the child's command.
    let command = format!(
        "'{}' > '{}' 2>&1; printf '%s\\n' \"$?\" > '{}'",
        child.display(),
        log_file.display(),
        exit_file.display()
    );
    let status = bash(&["-c", &command]);
    assert!(status.status.success(), "{command}");
    assert_eq!(
        fs::read_to_string(&exit_file).unwrap().trim(),
        "7",
        "the pane must record the suite's exit status, not a literal $?"
    );
}

#[test]
fn the_workflow_uploads_only_the_trace_the_screens_logs_and_the_versions() {
    let workflow = workflow();
    let upload = workflow
        .split("upload-artifact")
        .nth(1)
        .expect("the workflow uploads an artifact");
    for expected in [
        "artifacts/*.screens.log",
        "artifacts/*.screens.trace",
        "artifacts/versions.txt",
    ] {
        assert!(upload.contains(expected), "the upload lost {expected}");
    }
    // Raw harness output must stay on the runner.
    for forbidden in ["run.err", "result.json", "artifact.txt", "update.log"] {
        assert!(
            !upload.contains(forbidden),
            "the upload must not include {forbidden}"
        );
    }
}

#[test]
fn the_workflow_files_one_issue_per_harness_and_failure_kind() {
    let workflow = workflow();
    let filing = workflow
        .split("File one issue per failed harness")
        .nth(1)
        .expect("the workflow files issues");
    // The title names the harness and the failure kind, so one title per pair.
    assert!(
        filing.contains(r#"title="[brgr live smoke] $short $kind""#),
        "the title must name the harness and the failure kind"
    );
    assert!(
        filing.contains("brgr-live-smoke-fp:"),
        "a marker in the body makes the issue findable"
    );
    assert!(
        filing.contains("gh issue list"),
        "an open issue is searched for before creating one"
    );
    assert!(
        filing.contains(".title =="),
        "the lookup matches the title, so a repeat failure finds its issue"
    );
    assert!(
        filing.contains("gh issue comment"),
        "a repeat failure comments instead of opening a second issue"
    );
    assert!(
        workflow.contains("issues: write"),
        "filing needs issue write permission"
    );
}

/// The filing step's lookup must find an open issue by title, and must not
/// treat "no match" as a match. jq's `// empty` on an empty list prints the
/// list, not nothing, so a wrong filter here commented on a nonexistent
/// issue instead of opening one.
#[test]
fn an_open_issues_number_is_only_used_when_there_is_one() {
    let block = {
        let text = workflow();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.contains("name: File one issue per failed harness"))
            .expect("the filing step exists");
        let run = lines[start..]
            .iter()
            .position(|line| line.trim() == "run: |")
            .map(|offset| start + offset)
            .expect("the filing step runs a script");
        let mut block = String::new();
        for line in &lines[run + 1..] {
            if line.starts_with("      - ") && !line.starts_with("        ") {
                break;
            }
            block.push_str(line.strip_prefix("          ").unwrap_or(line));
            block.push('\n');
        }
        block
    };

    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(root.path().join("artifacts")).unwrap();
    fs::write(
        root.path().join("artifacts/failures.jsonl"),
        r#"{"harness":"local.codex","kind":"run_failed","version":"0.169.0","class":"codex hung"}"#,
    )
    .unwrap();
    // A gh that honours --jq, so the test sees what the real one prints.
    fs::write(
        bin.join("gh"),
        r#"#!/bin/bash
echo "GH $*" >> "$GH_CALLS"
case "$1 $2" in
  "issue list")
    if [ "${GH_MODE:-none}" = existing ]; then
      json='[{"number":7,"title":"[brgr live smoke] codex run_failed"}]'
    else
      json='[]'
    fi
    jqexpr='.'
    prev=''
    for a in "$@"; do [ "$prev" = "--jq" ] && jqexpr="$a"; prev="$a"; done
    printf '%s' "$json" | jq -r "$jqexpr";;
  "issue create") echo "https://example.invalid/1";;
  "issue comment") echo commented;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    for name in ["jq", "shasum", "sha256sum", "cut", "date", "mktemp", "rm", "cat"] {
        if let Some(found) = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
        {
            std::os::unix::fs::symlink(found, bin.join(name)).unwrap();
        }
    }
    let calls = root.path().join("gh.calls");
    // `bash` may not be on the minimal PATH this step gets, so resolve the real
    // one from this machine's PATH and use it by absolute path.
    let real_bash = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("bash"))
        .find(|candidate| candidate.is_file())
        .expect("bash is installed");
    let run = |mode: &str| {
        let mut command = Command::new(&real_bash);
        command.arg(root.path().join("block.sh"));
        command.current_dir(root.path());
        command.env("PATH", &bin);
        command.env("GH_CALLS", &calls);
        command.env("GH_MODE", mode);
        command.env("GITHUB_REPOSITORY", "owner/name");
        command.env("GITHUB_SERVER_URL", "https://example.invalid");
        command.env("GITHUB_RUN_ID", "1");
        command.output().unwrap()
    };
    fs::write(root.path().join("block.sh"), &block).unwrap();
    assert!(
        block.contains("gh issue list") && block.contains("gh issue create"),
        "the extracted step is not the filing script:\n{block}"
    );

    let output = run("none");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}\nblock:\n{block}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let first = fs::read_to_string(&calls).unwrap_or_else(|error| {
        panic!(
            "gh was never called ({error}); stdout: {}\nstderr: {}\nblock:\n{block}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        first.contains("issue create --title [brgr live smoke] codex run_failed"),
        "no open issue should be created: {first}"
    );
    assert!(!first.contains("issue comment"), "{first}");

    fs::write(&calls, "").unwrap();
    let output = run("existing");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let second = fs::read_to_string(&calls).unwrap();
    assert!(
        second.contains("issue comment 7 "),
        "an open issue is commented on by number: {second}"
    );
    assert!(!second.contains("issue create"), "{second}");
}

#[test]
fn the_script_reproduces_the_manual_tier_one_round() {
    let source = script_source();
    for step in [
        "--home",            // an isolated control home
        "harness add",       // registration, and the registration is a real run
        "--mode pane",       // pane mode, as the manual round uses
        "--json watch",      // wait for the sealed result
        "--json result",     // read the sealed artifacts
        "--json accept",     // the owner decision
        "verify_cleanup",    // the pane closed and the worktree went away
        "workspace_present", // the worktree check reads the recorded state
        "session.cleanup",   // the pane check reads the recorded cleanup
        "BRGR_SCREEN_TRACE=1",
    ] {
        assert!(source.contains(step), "the script is missing {step}");
    }
    assert!(
        source.contains("Read README.md"),
        "the objective is the tiny README task"
    );
}

/// The script's own control flow, with `brgr`, `herdr` and the harness CLIs
/// replaced by fakes. A real round needs logins; the ordering, the exit status
/// and the names in the failure record do not.
mod control_flow {
    use super::*;

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A bin directory with a fake harness and the fakes `brgr`, `herdr`, and
    /// the helpers the script calls on the way.
    struct Fakes {
        dir: tempfile::TempDir,
        bin: PathBuf,
        root: PathBuf,
        artifacts: PathBuf,
    }

    impl Fakes {
        fn new(brgr: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            fs::create_dir_all(&bin).unwrap();
            let root = dir.path().join("run");
            fs::create_dir_all(root.join("home/runs")).unwrap();
            let artifacts = dir.path().join("artifacts");
            fs::create_dir_all(&artifacts).unwrap();
            write_executable(&bin.join("brgr"), brgr);
            // The harness the script runs must exist and report a version.
            write_executable(
                &bin.join("claude"),
                "#!/bin/sh\ncase \"$1\" in --version) echo 'fake 1.0';; esac\nexit 0\n",
            );
            // Herdr is only asked whether a pane is still there; a missing pane
            // answers with a non-zero status.
            write_executable(&bin.join("herdr"), "#!/bin/sh\nexit 1\n");
            // A screens log and trace the script should collect.
            fs::write(root.join("home/runs/t.screens.log"), "0 w1:p1 rule n\n").unwrap();
            fs::write(root.join("home/runs/t.screens.trace"), "0 pane=w1:p1\n").unwrap();
            Self {
                dir,
                bin,
                root,
                artifacts,
            }
        }

        fn run(&self, extra: &[&str]) -> std::process::Output {
            // The script's PATH holds only the fakes, so name bash absolutely.
            let bash = find_in_path("bash").unwrap_or_else(|| PathBuf::from("/bin/bash"));
            let mut command = Command::new(bash);
            command.arg(script());
            command.args([
                "--inside-pane",
                "--root",
                self.root.to_str().unwrap(),
                "--artifacts",
                self.artifacts.to_str().unwrap(),
                "--repo",
                repository_root().to_str().unwrap(),
                "--brgr",
                self.bin.join("brgr").to_str().unwrap(),
            ]);
            command.args(extra);
            // Only the fakes, so a harness installed on the machine running the
            // tests cannot change what the script finds. The ordinary tools the
            // script calls are linked from the real PATH.
            for needed in [
                "git", "jq", "cargo", "sh", "uname", "date", "cut", "sed", "grep", "tr", "head",
                "tail", "cat", "cp", "rm", "mkdir", "sleep", "mktemp", "dirname", "basename",
                "perl", "env", "wc", "sort", "printf",
            ] {
                if let Some(found) = find_in_path(needed) {
                    let link = self.bin.join(needed);
                    if !link.exists() {
                        std::os::unix::fs::symlink(found, &link).unwrap();
                    }
                }
            }
            command.env("PATH", &self.bin);
            command.env("HERDR_ENV", "1");
            command.env("HERDR_PANE_ID", "w1:p1");
            command.env("BRGR_LIVE_SMOKE_HARNESSES", "local.claude-code");
            command.env("BRGR_LIVE_SMOKE_SKIP_UPDATE", "1");
            // A fake status is already settled, so the poll exits at once; the
            // bound only keeps a broken fake from hanging the suite.
            command.env("BRGR_LIVE_SMOKE_CLEANUP_WAIT", "2");
            command.current_dir(self.dir.path());
            command.output().expect("bash runs the script")
        }
    }

    fn find_in_path(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    }

    /// A brgr that answers every call the script makes for a clean run.
    const SUCCESSFUL_BRGR: &str = r#"#!/bin/sh
set -eu
if [ "$1" = --version ]; then echo 'brgr 9.9.9'; exit 0; fi
home=''
while [ $# -gt 0 ]; do
  case "$1" in
    --home) home=$2; shift 2;;
    --json) shift;;
    *) break;;
  esac
done
command=$1
shift
case "$command" in
  harness) echo '{"harness_id":"local.claude-code"}';;
  run) echo '{"task_id":"11111111-1111-4111-8111-111111111111"}';;
  watch) echo '{"event":"result","outcome":"candidate"}';;
  result) echo '{"result":{"outcome":"candidate"},"artifacts":[{"text":"LIVE-SMOKE-OK"}]}';;
  accept) echo '{"verdict":"accepted"}';;
  status) echo '{"workspace_present":false,"session":{"cleanup":"closed","pane":"w1:p2"}}';;
esac
exit 0
"#;

    #[test]
    fn a_clean_round_records_the_versions_and_reports_success() {
        let fakes = Fakes::new(SUCCESSFUL_BRGR);
        let output = fakes.run(&[]);
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let summary = fs::read_to_string(fakes.artifacts.join("summary.txt")).unwrap();
        assert!(summary.contains("OK local.claude-code"), "{summary}");
        let versions = fs::read_to_string(fakes.artifacts.join("versions.txt")).unwrap();
        // The line is what `brgr --version` prints, so exactly one "brgr".
        assert!(
            versions
                .lines()
                .any(|line| line.starts_with("brgr 9.9.9 (")),
            "{versions}"
        );
        assert!(
            versions.contains("claude-code update=skipped fake 1.0"),
            "{versions}"
        );
        assert!(
            fakes.artifacts.join("t.screens.log").exists(),
            "the screens log is uploaded"
        );
        assert!(
            fakes.artifacts.join("t.screens.trace").exists(),
            "the trace is uploaded"
        );
        // Every run passed, so nothing is filed.
        assert_eq!(
            fs::read_to_string(fakes.artifacts.join("failures.jsonl")).unwrap(),
            ""
        );
    }

    #[test]
    fn a_run_that_does_not_return_the_readme_line_fails_and_is_named() {
        let brgr = SUCCESSFUL_BRGR.replace(
            r#"echo '{"result":{"outcome":"candidate"},"artifacts":[{"text":"LIVE-SMOKE-OK"}]}'"#,
            r#"echo '{"result":{"outcome":"candidate"},"artifacts":[{"text":"something else"}]}'"#,
        );
        let fakes = Fakes::new(&brgr);
        let output = fakes.run(&[]);
        assert!(
            !output.status.success(),
            "a wrong artifact must fail the run"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("claude-code:artifact"), "{stderr}");
        let failures = fs::read_to_string(fakes.artifacts.join("failures.jsonl")).unwrap();
        let line: serde_json::Value =
            serde_json::from_str(failures.lines().next().unwrap()).unwrap();
        assert_eq!(line["harness"], "local.claude-code");
        assert_eq!(line["kind"], "artifact");
    }

    /// A failed run's error carries a pane's screen after `last screen:`. The
    /// failure record must not, or the workflow would put it in an issue.
    #[test]
    fn a_failed_runs_screen_text_never_reaches_the_failure_record() {
        let brgr = SUCCESSFUL_BRGR.replace(
            r#"result) echo '{"result":{"outcome":"candidate"},"artifacts":[{"text":"LIVE-SMOKE-OK"}]}';;"#,
            r#"result) echo '{"result":{"outcome":"failed","error":"codex hung after 180s | run failed: no progress; last screen: Do you trust /Users/example/project?"},"artifacts":[]}';;"#,
        );
        let fakes = Fakes::new(&brgr);
        let output = fakes.run(&[]);
        assert!(!output.status.success());
        let failures = fs::read_to_string(fakes.artifacts.join("failures.jsonl")).unwrap();
        assert!(
            !failures.contains("Do you trust"),
            "the screen reached the failure record: {failures}"
        );
        assert!(!failures.contains("/Users/example"), "{failures}");
        let line: serde_json::Value =
            serde_json::from_str(failures.lines().next().unwrap()).unwrap();
        assert_eq!(line["kind"], "run_failed");
    }

    #[test]
    fn a_missing_harness_is_reported_rather_than_skipped() {
        let fakes = Fakes::new(SUCCESSFUL_BRGR);
        // A harness id the script knows but whose executable is not installed.
        fs::remove_file(fakes.bin.join("claude")).unwrap();
        let output = fakes.run(&[]);
        assert!(!output.status.success());
        let failures = fs::read_to_string(fakes.artifacts.join("failures.jsonl")).unwrap();
        assert!(failures.contains("harness_missing"), "{failures}");
    }
}
