//! Execute a registered native TUI with its terminal streams inherited.
use crate::write_json_atomic;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

mod opencode;

#[derive(Serialize, Deserialize)]
pub(crate) struct NativeLaunch {
    pub(crate) executable: PathBuf,
    pub(crate) digest: String,
    pub(crate) argv: Vec<String>,
    pub(crate) workspace: PathBuf,
    pub(crate) environment: BTreeMap<String, String>,
    /// The harness's allowlisted variables. The pane's own shell supplies
    /// these, so a login made after the caller started is the one used.
    #[serde(default)]
    pub(crate) pane_environment: Vec<String>,
    pub(crate) state: PathBuf,
    /// `OpenCode` 2 TUI resumes a model-selected session instead of taking a
    /// `--model` flag. Absent in old launch files and for all other harnesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) opencode_model: Option<String>,
    /// argv[0] Herdr matches on when the registered file's name is not the
    /// harness kind. Absent in old launch files and when the file is already
    /// named for its kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) argv0: Option<String>,
}

/// What a terminal program reads to decide whether it may use colour. They
/// describe the pane the worker runs in, not the process that started the task:
/// that process may be a tool shell with `TERM=dumb`, or have no terminal at all.
const TERMINAL_ENVIRONMENT: [&str; 3] = ["TERM", "COLORTERM", "NO_COLOR"];

/// The variables the pane's own shell supplies: the harness's allowlist plus the
/// terminal's. Without `TERM` and `COLORTERM` a worker's TUI falls back to a few
/// colours, or none.
pub(crate) fn pane_variables(allowlist: &[String]) -> Vec<String> {
    let mut names = allowlist.to_vec();
    for name in TERMINAL_ENVIRONMENT {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
    }
    names
}

/// Takes each named variable from the pane's shell, and drops the caller's copy
/// when the pane does not set it. A caller started before the user replaced a
/// key would otherwise hand the worker the stale one.
fn take_from_pane(
    environment: &mut BTreeMap<String, String>,
    names: &[String],
    pane: impl Fn(&str) -> Option<String>,
) {
    for name in names {
        match pane(name) {
            Some(value) => {
                environment.insert(name.clone(), value);
            }
            None => {
                environment.remove(name);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct NativeState {
    pub(crate) pid: u32,
    pub(crate) birth: String,
    pub(crate) phase: String,
    pub(crate) exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) startup_error: Option<String>,
}

/// argv[0] Herdr uses to identify the agent. The registered file is still
/// executed; only the name it appears under changes when it is not already
/// the harness kind.
pub(crate) fn launch_argv0(executable: &Path, kind: &str) -> Option<String> {
    match executable.file_name() {
        Some(name) if name == kind => None,
        _ => Some(kind.to_owned()),
    }
}

fn argv0_is_harness_kind(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn native_command(launch: &NativeLaunch) -> Result<Command> {
    if let Some(argv0) = &launch.argv0
        && !argv0_is_harness_kind(argv0)
    {
        bail!("native launch argv0 must be a harness kind");
    }
    let mut command = Command::new(&launch.executable);
    command
        .args(&launch.argv)
        .current_dir(&launch.workspace)
        .env_clear()
        .envs(&launch.environment);
    if let Some(argv0) = &launch.argv0 {
        command.arg0(argv0);
    }
    Ok(command)
}

pub(crate) fn run(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 65_536 {
        bail!("native launch must be a bounded regular file");
    }
    let mut launch: NativeLaunch = serde_json::from_slice(&fs::read(path)?)?;
    // The host is in the new terminal. Do not give the native worker the
    // controller's inherited parent pane variables from its launch envelope.
    for key in [
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_BIN_PATH",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
    ] {
        if let Ok(value) = std::env::var(key) {
            launch.environment.insert(key.to_owned(), value);
        }
    }
    let names = launch.pane_environment.clone();
    take_from_pane(&mut launch.environment, &names, |name| {
        std::env::var(name).ok()
    });
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(launch.state.with_extension("lock"))?;
    lock.try_lock()
        .context("this native TUI already has a host")?;
    let bytes = fs::read(&launch.executable)?;
    let mut digest = String::new();
    for byte in Sha256::digest(bytes) {
        write!(&mut digest, "{byte:02x}")?;
    }
    if launch.digest.trim_start_matches("sha256:") != digest {
        bail!("registered native executable changed before TUI launch");
    }
    if let Some(model) = launch.opencode_model.clone()
        && let Err(error) = opencode::prepare(&mut launch, &model)
    {
        write_json_atomic(
            &launch.state,
            &NativeState {
                pid: std::process::id(),
                birth: String::new(),
                phase: "failed".into(),
                exit_code: Some(1),
                startup_error: Some(format!("{error:#}")),
            },
        )?;
        return Err(error);
    }
    let mut child = native_command(&launch)?
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("could not start registered native TUI")?;
    let pid = child.id();
    let birth = crate::supervision::ps_field(&pid.to_string(), "lstart")?;
    write_json_atomic(
        &launch.state,
        &NativeState {
            pid,
            birth: birth.clone(),
            phase: "running".into(),
            exit_code: None,
            startup_error: None,
        },
    )?;
    let status = child.wait()?;
    write_json_atomic(
        &launch.state,
        &NativeState {
            pid,
            birth,
            phase: "exited".into(),
            exit_code: status.code(),
            startup_error: None,
        },
    )?;
    Ok(())
}

pub(crate) fn alive(path: &Path) -> bool {
    let Some(state) = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<NativeState>(&b).ok())
    else {
        return false;
    };
    state.phase == "running"
        && crate::supervision::ps_field(&state.pid.to_string(), "lstart")
            .is_ok_and(|birth| birth == state.birth)
}

/// Stops everything under a running host. An agent's tools can sit in their own
/// process groups, so closing the pane ends the agent and leaves them running. A
/// Codex tool runs in Codex's shared daemon and is out of reach here.
pub(crate) fn stop_descendants(path: &Path) {
    if !alive(path) {
        return;
    }
    let Some(state) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<NativeState>(&bytes).ok())
    else {
        return;
    };
    let Ok(output) = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
    else {
        return;
    };
    let table = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect::<Vec<(u32, u32)>>();
    for pid in descendants(&table, state.pid) {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
}

/// Every process below `root` in a `(pid, parent)` table, children before
/// their own children.
fn descendants(table: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut next = vec![root];
    while let Some(parent) = next.pop() {
        for (pid, _) in table
            .iter()
            .filter(|(pid, ppid)| *ppid == parent && *pid != root)
        {
            if !found.contains(pid) {
                found.push(*pid);
                next.push(*pid);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendants_reach_every_level_and_nothing_else() {
        let table = [(10, 1), (11, 10), (12, 11), (13, 12), (20, 1), (21, 20)];
        let mut found = descendants(&table, 10);
        found.sort_unstable();
        assert_eq!(found, vec![11, 12, 13]);
        assert!(descendants(&table, 13).is_empty());
    }

    #[test]
    fn the_pane_shell_supplies_harness_variables_over_the_callers() {
        let mut environment = BTreeMap::from([
            ("COMMAND_CODE_API_KEY".to_owned(), "stale".to_owned()),
            ("OLD_TOKEN".to_owned(), "stale".to_owned()),
            ("BRGR_HOME".to_owned(), "/brgr".to_owned()),
        ]);
        let names = vec![
            "COMMAND_CODE_API_KEY".to_owned(),
            "OLD_TOKEN".to_owned(),
            "PATH".to_owned(),
        ];
        take_from_pane(&mut environment, &names, |name| match name {
            "COMMAND_CODE_API_KEY" => Some("fresh".to_owned()),
            "PATH" => Some("/bin".to_owned()),
            _ => None,
        });
        assert_eq!(environment["COMMAND_CODE_API_KEY"], "fresh");
        assert_eq!(environment["PATH"], "/bin");
        assert!(!environment.contains_key("OLD_TOKEN"));
        assert_eq!(environment["BRGR_HOME"], "/brgr");
    }

    #[test]
    fn a_workers_terminal_comes_from_its_pane_not_from_the_process_that_started_it() {
        let allowlist = ["HOME".to_owned(), "PATH".to_owned()];
        let names = pane_variables(&allowlist);
        assert_eq!(names[..2], allowlist);
        for terminal in ["TERM", "COLORTERM", "NO_COLOR"] {
            assert!(names.iter().any(|name| name == terminal), "{terminal}");
        }
        assert_eq!(
            pane_variables(&names),
            names,
            "adding twice changes nothing"
        );

        // The starting process was a tool shell: TERM=dumb and a NO_COLOR it
        // set for its own output. The pane has a real terminal.
        let mut environment = BTreeMap::from([
            ("TERM".to_owned(), "dumb".to_owned()),
            ("NO_COLOR".to_owned(), "1".to_owned()),
            ("HOME".to_owned(), "/home/u".to_owned()),
        ]);
        take_from_pane(&mut environment, &names, |name| match name {
            "TERM" => Some("xterm-256color".to_owned()),
            "COLORTERM" => Some("truecolor".to_owned()),
            "HOME" => Some("/home/u".to_owned()),
            _ => None,
        });
        assert_eq!(environment["TERM"], "xterm-256color");
        assert_eq!(environment["COLORTERM"], "truecolor");
        assert!(
            !environment.contains_key("NO_COLOR"),
            "the caller's NO_COLOR is not the pane's"
        );
    }

    #[test]
    fn a_launch_written_before_pane_environment_still_reads() {
        let launch: NativeLaunch = serde_json::from_str(
            r#"{"executable":"/bin/echo","digest":"x","argv":[],"workspace":"/","environment":{},"state":"/s"}"#,
        )
        .unwrap();
        assert!(launch.pane_environment.is_empty());
        assert!(launch.argv0.is_none());
    }

    fn sh_launch(argv0: Option<&str>) -> NativeLaunch {
        NativeLaunch {
            executable: PathBuf::from("/bin/sh"),
            digest: String::new(),
            argv: vec!["-c".into(), r#"printf %s "$0""#.into()],
            workspace: PathBuf::from("/"),
            environment: BTreeMap::new(),
            pane_environment: Vec::new(),
            state: PathBuf::from("/s"),
            opencode_model: None,
            argv0: argv0.map(str::to_owned),
        }
    }

    #[test]
    fn a_versioned_executable_runs_under_its_harness_kind() {
        assert_eq!(
            launch_argv0(Path::new("/x/claude/versions/2.1.325"), "claude").as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn an_executable_already_named_for_its_kind_keeps_its_path_as_argv0() {
        assert_eq!(
            launch_argv0(Path::new("/opt/brgr/bin/codex"), "codex"),
            None
        );
        assert_eq!(
            launch_argv0(Path::new("/opt/brgr/bin/opencode"), "opencode"),
            None
        );
    }

    fn program_and_args(command: &Command) -> (String, Vec<String>) {
        (
            command.get_program().to_string_lossy().into_owned(),
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        )
    }

    fn captured_stdout(mut command: Command) -> Vec<u8> {
        let output = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn the_host_starts_the_registered_file_with_the_requested_argv0() {
        let named = sh_launch(Some("claude"));
        let command = native_command(&named).unwrap();
        let (program, args) = program_and_args(&command);
        assert_eq!(program, "/bin/sh");
        assert_eq!(args, named.argv);
        assert_eq!(captured_stdout(command), b"claude");

        let unnamed = sh_launch(None);
        let command = native_command(&unnamed).unwrap();
        let (program, args) = program_and_args(&command);
        assert_eq!(program, "/bin/sh");
        assert_eq!(args, unnamed.argv);
        assert_eq!(captured_stdout(command), b"/bin/sh");
    }

    #[test]
    fn a_launch_file_with_an_unsafe_argv0_is_refused() {
        for argv0 in ["../x", "", "a/b"] {
            assert!(
                native_command(&sh_launch(Some(argv0))).is_err(),
                "{argv0:?}"
            );
        }
    }

    #[test]
    fn a_launch_written_before_argv0_still_reads() {
        let launch: NativeLaunch = serde_json::from_str(
            r#"{"executable":"/bin/echo","digest":"x","argv":[],"workspace":"/","environment":{},"state":"/s"}"#,
        )
        .unwrap();
        assert!(launch.argv0.is_none());
        let json = serde_json::to_string(&launch).unwrap();
        assert!(
            !json.contains("argv0"),
            "a launch with argv0: None serializes without the key: {json}"
        );
    }

    #[test]
    fn a_changed_executable_is_refused_before_spawn_even_with_argv0() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("worker");
        fs::write(&executable, b"not-the-registered-bytes").unwrap();
        let state = temp.path().join("state.json");
        let path = temp.path().join("launch.json");
        let launch = NativeLaunch {
            executable,
            digest: "sha256:deadbeef".into(),
            argv: vec!["-c".into(), "echo spawned".into()],
            workspace: temp.path().to_owned(),
            environment: BTreeMap::new(),
            pane_environment: Vec::new(),
            state,
            opencode_model: None,
            argv0: Some("claude".into()),
        };
        fs::write(&path, serde_json::to_vec(&launch).unwrap()).unwrap();
        let error = run(&path).unwrap_err().to_string();
        assert!(
            error.contains("registered native executable changed before TUI launch"),
            "{error}"
        );
    }
}
