//! What brgr presses so a worker is never left for a person to click.
//!
//! Orchestrating through brgr is an unconditional-trust contract, so a trust
//! prompt, a "continue" notice, an update offer, or an update that has already
//! been applied is answered here. A screen no rule covers is reported with its
//! text and given a deadline instead of waiting forever.

use std::{
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};

use super::{
    Herdr,
    native::{squash, trust_workspace_matches},
};
use crate::{Paths, cli::PaneRunArgs};

/// How long the agent gets to redraw after a key press before the screen is
/// read again. Without it the same prompt is answered twice.
const REDRAW: Duration = Duration::from_millis(1500);

/// How long an answered screen may stand unchanged before its rule is pressed
/// again. Long against any redraw; five presses still fit inside `SCREEN_DEADLINE`.
const REPRESS_AFTER: Duration = Duration::from_secs(30);

/// How long a screen no rule covers may stand before the run fails.
pub(super) const SCREEN_DEADLINE: Duration = Duration::from_mins(3);

/// The unknown-screen deadline, shortened for tests only.
fn screen_deadline() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(millis) = std::env::var("BRGR_TEST_SCREEN_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(millis);
    }
    SCREEN_DEADLINE
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Screen {
    /// No dialog brgr recognizes.
    Clear,
    /// A dialog a rule answers with these keys.
    Resolve {
        rule: &'static str,
        keys: Vec<&'static str>,
    },
    /// A menu or dialog no rule answers; carries its first lines.
    Unknown(String),
    /// A dialog a rule already answered that still stands unchanged; nothing was pressed.
    Answered { rule: &'static str },
}

/// Classifies a pane's visible text. Pure: it never reads or presses anything.
pub(super) fn classify(screen: &str, workspace: &Path) -> Screen {
    if trust_workspace_matches(screen, workspace) {
        let options = menu_options(screen);
        let yes = options
            .iter()
            .position(|option| option.text.starts_with("Yes, I trust this folder"));
        let keys = if let (Some(selected), Some(yes)) =
            (options.iter().position(|option| option.selected), yes)
        {
            // A numbered menu: move to "Yes" from wherever the cursor is.
            let mut keys = keys_between(selected, yes);
            keys.push("enter");
            keys
        } else {
            let flat = squash(screen);
            let selected_yes = ["❯", "›", ">"]
                .iter()
                .any(|cursor| flat.contains(&format!("{cursor}Yes,Itrustthisfolder")));
            if selected_yes {
                vec!["enter"]
            } else {
                vec!["down", "enter"]
            }
        };
        return Screen::Resolve {
            rule: "workspace-trust",
            keys,
        };
    }
    if let Some(resolution) = codex_trust(screen, workspace, || {
        crate::workspace::primary_checkout(workspace)
    }) {
        return resolution;
    }
    if let Some(resolution) = copilot_trust(screen, workspace) {
        return resolution;
    }
    if let Some(resolution) = cursor_trust(screen, workspace) {
        return resolution;
    }
    let lower = screen.to_lowercase();
    // A trust prompt for some other folder is not the task's to accept.
    if squash(&lower).contains("trustthisfolder") {
        return Screen::Unknown(head(screen));
    }
    // The harness updated itself and wants a restart. The dialog carries an
    // `esc` affordance; Enter ("ok") is what quit OpenCode.
    if lower.contains("update complete")
        && lower.contains("restart")
        && screen
            .lines()
            .any(|line| matches!(line.trim().to_lowercase().as_str(), "ok" | "esc"))
    {
        return Screen::Resolve {
            rule: "update-applied",
            keys: vec!["esc"],
        };
    }
    if let Some(resolution) = opencode_web_search(&lower) {
        return resolution;
    }
    let options = menu_options(screen);
    let selected = options.iter().position(|option| option.selected);
    if let (Some(selected), Some((rule, target))) = (selected, accepting_option(&lower, &options)) {
        let mut keys = keys_between(selected, target);
        keys.push("enter");
        return Screen::Resolve { rule, keys };
    }
    // An update menu comes first: its highlighted option is "Update now", so a
    // stray "press Enter" line on the same screen must not become Enter.
    if let Some(selected) = selected
        && is_update_offer(&lower)
        && let Some(skip) = options.iter().position(|option| is_skip(&option.text))
    {
        let mut keys = keys_between(selected, skip);
        keys.push("enter");
        return Screen::Resolve {
            rule: "update-offer-skip",
            keys,
        };
    }
    // Claude Code's first-run offer has its cursor on "Yes, try it".
    if let Some(selected) = selected
        && lower.contains("try the new fullscreen renderer?")
        && let Some(skip) = options.iter().position(|option| is_skip(&option.text))
    {
        let mut keys = keys_between(selected, skip);
        keys.push("enter");
        return Screen::Resolve {
            rule: "renderer-offer-skip",
            keys,
        };
    }
    if let Some(resolution) = lsp_plugin_offer(&lower, selected, &options) {
        return resolution;
    }
    if selected.is_none() {
        return if has_continue_notice(screen) {
            Screen::Resolve {
                rule: "continue-notice",
                keys: vec!["enter"],
            }
        } else {
            Screen::Clear
        };
    }
    Screen::Unknown(head(screen))
}

/// `OpenCode`'s web-search prompt: Esc dismisses. Allow would send queries to
/// third-party providers; Disable may change `OpenCode`'s global settings.
fn opencode_web_search(lower: &str) -> Option<Screen> {
    lower
        .contains("allow opencode to search the web")
        .then_some(Screen::Resolve {
            rule: "opencode-web-search-dismiss",
            keys: vec!["esc"],
        })
}

/// Claude Code's LSP plugin recommendation: decline without installing.
/// Installing a plugin runs code and changes user-level settings.
fn lsp_plugin_offer(
    lower: &str,
    selected: Option<usize>,
    options: &[MenuOption],
) -> Option<Screen> {
    let selected = selected?;
    if !lower.contains("would you like to install this lsp plugin?") {
        return None;
    }
    let skip = options
        .iter()
        .position(|option| option.text.to_lowercase().starts_with("no, not now"))?;
    let mut keys = keys_between(selected, skip);
    keys.push("enter");
    Some(Screen::Resolve {
        rule: "lsp-plugin-offer-skip",
        keys,
    })
}

/// A harness offering to update itself, not an agent asking about updating
/// something: "update" or "upgrade" with an offer's wording or a version.
fn is_update_offer(lower: &str) -> bool {
    (lower.contains("update") || lower.contains("upgrade"))
        && (lower.contains("available")
            || lower.contains("new version")
            || lower.contains("newer version")
            || has_version_number(lower))
}

/// A dotted version such as `0.2.0` or `v2.0.15`.
fn has_version_number(text: &str) -> bool {
    text.split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .any(|token| {
            let parts: Vec<&str> = token.trim_matches('.').split('.').collect();
            parts.len() >= 3 && parts.iter().all(|part| !part.is_empty())
        })
}

/// The cursor presses that move from option `from` to option `to`.
fn keys_between(from: usize, to: usize) -> Vec<&'static str> {
    let key = if to > from { "down" } else { "up" };
    std::iter::repeat_n(key, to.abs_diff(from)).collect()
}

/// A harness's own gates in front of a run, answered by accepting: Claude Code's
/// Bypass Permissions warning and newly found MCP server, and Codex's review of
/// new hooks, which `brgr integrate codex install` is what adds.
fn accepting_option(lower: &str, options: &[MenuOption]) -> Option<(&'static str, usize)> {
    let position = |prefix: &str| {
        options
            .iter()
            .position(|option| option.text.to_lowercase().starts_with(prefix))
    };
    if lower.contains("bypass permissions mode") {
        return position("yes, i accept").map(|at| ("bypass-warning", at));
    }
    if lower.contains("hooks need review") {
        return position("trust all").map(|at| ("codex-hooks-trust", at));
    }
    if lower.contains("mcp server") {
        // The single-server choice first; "use this and all future" only if it
        // is all there is.
        return position("use this mcp server")
            .or_else(|| position("use this"))
            .map(|at| ("mcp-trust", at));
    }
    None
}

fn has_continue_notice(screen: &str) -> bool {
    screen.lines().any(|line| {
        let line = line
            .trim()
            .trim_matches(['│', '┃', '║'])
            .trim()
            .trim_start_matches(['•', '·', '*', '-', '›', '❯'])
            .trim_start()
            .to_lowercase();
        let notices = [
            "press enter to continue",
            "press return to continue",
            "press any key to continue",
        ];
        notices.iter().any(|notice| {
            line.match_indices(notice).any(|(start, _)| {
                let before = line[..start].trim_end();
                let after = line[start + notice.len()..].trim();
                (before.is_empty() || before.ends_with(['.', '!', '?']))
                    && after.chars().all(is_closing_punctuation)
            })
        })
    })
}

fn is_closing_punctuation(character: char) -> bool {
    character.is_ascii_punctuation()
        || matches!(
            character,
            '…' | '⋯' | '”' | '’' | '»' | '。' | '！' | '？' | '、'
        )
}

/// Codex's own folder prompt ("Trust this folder? ... 1. Trust and continue").
/// Accepted only for the task's workspace, like Claude's.
/// `primary` runs `git` and is asked only for a worktree prompt: every poll of
/// every pane classifies its screen.
fn codex_trust(
    screen: &str,
    workspace: &Path,
    primary: impl FnOnce() -> Option<PathBuf>,
) -> Option<Screen> {
    let flat = squash(screen);
    if !flat.contains("Trustthisfolder?") || !flat.contains("Trustandcontinue") {
        return None;
    }
    let workspace = squash(&workspace.to_string_lossy());
    let primary = flat
        .contains("repositoryroot:")
        .then(primary)
        .flatten()
        .map(|primary| squash(&primary.to_string_lossy()));
    // The path must end where the prompt's own sentence begins, so a longer
    // path ("/repo/task-evil", "/repo/task/sub") is not the task's workspace.
    // In a task worktree Codex adds that trust applies to the repository
    // root, which must then be exactly the repository the worktree came from.
    let shown = flat.split_once("Folderaccess").is_some_and(|(_, after)| {
        after
            .strip_prefix(workspace.trim_end_matches('/'))
            .map(|rest| rest.trim_start_matches('/'))
            .is_some_and(|rest| {
                rest.starts_with("Trustthisfolder")
                    || rest.starts_with("Note:")
                        && rest
                            .split_once("repositoryroot:")
                            .and_then(|(_, root)| root.split_once("Trustthisfolder"))
                            .is_some_and(|(root, _)| {
                                primary.as_deref().is_some_and(|primary| {
                                    root.trim_end_matches('/') == primary.trim_end_matches('/')
                                })
                            })
            })
    });
    Some(trust_choice(screen, shown, "codex-trust", |text| {
        text.starts_with("Trust and continue")
    }))
}

/// GitHub Copilot CLI's folder prompt ("Confirm folder trust … Do you trust the
/// files in this folder? 1. Yes"). Accepted only for the task's workspace: the
/// path is drawn wrapped inside a box, so it is compared with the borders
/// removed, and it must be exactly the workspace. "Yes, and remember this
/// folder" is never chosen; a task worktree does not outlive its task.
fn copilot_trust(screen: &str, workspace: &Path) -> Option<Screen> {
    let flat: String = screen
        .chars()
        .filter(|character| {
            !character.is_whitespace() && !matches!(character, '│' | '╭' | '╮' | '╰' | '╯' | '─')
        })
        .collect();
    if !flat.contains("Confirmfoldertrust") || !flat.contains("Doyoutrustthefilesinthisfolder?") {
        return None;
    }
    let workspace = squash(&workspace.to_string_lossy());
    let shown = flat
        .split_once("Confirmfoldertrust")
        .and_then(|(_, after)| after.split_once("Copilotcanread"))
        .is_some_and(|(path, _)| path.trim_end_matches('/') == workspace.trim_end_matches('/'));
    Some(trust_choice(screen, shown, "copilot-trust", |text| {
        text == "Yes"
    }))
}

/// Cursor Agent's "Workspace Trust Required" box. It has no menu: it names the
/// key, `[a] Trust this workspace`, and `[q] Quit`. Accepted only for the task's
/// workspace; the path is wrapped inside the box, so it is compared with the
/// borders removed and must be exactly the workspace. Any other directory is a
/// screen no rule answers, never a `Clear` one.
fn cursor_trust(screen: &str, workspace: &Path) -> Option<Screen> {
    let flat: String = screen
        .chars()
        .filter(|character| {
            !character.is_whitespace() && !matches!(character, '│' | '╭' | '╮' | '╰' | '╯' | '─')
        })
        .collect();
    if !flat.contains("WorkspaceTrustRequired") || !flat.contains("[a]Trustthisworkspace") {
        return None;
    }
    // Once answered, the box stays drawn above the agent's own screen with this
    // line in place of the key hint; pressing again would type into its input.
    if flat.contains("Trustingworkspace") {
        return None;
    }
    let workspace = squash(&workspace.to_string_lossy());
    let shown = flat
        .split_once("Doyoutrustthecontentsofthisdirectory?")
        .and_then(|(_, after)| after.split_once("[a]Trustthisworkspace"))
        .is_some_and(|(path, _)| {
            // A wide pane draws a selection marker before `[a]`, after the path.
            let path = path.trim_end_matches(['▶', '▸', '❯', '›', '>']);
            path.trim_end_matches('/') == workspace.trim_end_matches('/')
        });
    Some(if shown {
        Screen::Resolve {
            rule: "cursor-trust",
            keys: vec!["a"],
        }
    } else {
        Screen::Unknown(head(screen))
    })
}

/// Answers a harness's folder-trust menu: the option `accept` names when the
/// prompt is for the task's own folder (`shown`), otherwise the screen is one
/// no rule answers.
fn trust_choice(
    screen: &str,
    shown: bool,
    rule: &'static str,
    accept: impl Fn(&str) -> bool,
) -> Screen {
    let options = menu_options(screen);
    let target = options.iter().position(|option| accept(&option.text));
    let (true, Some(target)) = (shown, target) else {
        return Screen::Unknown(head(screen));
    };
    let selected = options
        .iter()
        .position(|option| option.selected)
        .unwrap_or(target);
    let mut keys = keys_between(selected, target);
    keys.push("enter");
    Screen::Resolve { rule, keys }
}

/// Presses what the rule table says for this pane, unless the agent is working.
///
/// A working agent's own output can contain `› 1.`-style lines, so nothing is
/// pressed while Herdr reports `working`. The answered screen lives in the
/// pane/run [`AnswerMemory`] so every stage of one run shares it.
pub(super) fn resolve(
    herdr: &Herdr,
    pane: &str,
    run: &PaneRunArgs,
    paths: &Paths,
    status: Option<&str>,
    answers: &mut AnswerMemory,
    stage: &str,
) -> Result<Screen> {
    let workspace = run.workspace.as_path();
    if status == Some("working") {
        answers.forget();
        trace(paths, run, pane, stage, status, None, "working", false);
        return Ok(Screen::Clear);
    }
    let Some(screen) = herdr.screen(pane) else {
        trace(paths, run, pane, stage, status, None, "unreadable", false);
        return Ok(Screen::Clear); // unreadable is not "changed": keep the answer
    };
    let outcome = classify(&screen, workspace);
    if let Screen::Resolve { rule, keys } = &outcome {
        let rule = *rule;
        let identity = dialog_identity(rule, &screen, workspace);
        let selection = (rule == "workspace-trust")
            .then(|| trust_selection(&screen))
            .flatten();
        // A standing trust dialog whose cursor moved after the press applied
        // only the cursor key: finish it with the keys the current selection
        // needs instead of waiting out the hold. A cursor on "No, exit" is
        // never pressed.
        let partially_applied = rule == "workspace-trust"
            && selection.is_some()
            && answers.selection().is_some()
            && selection.as_deref() != answers.selection()
            && selection.as_deref() != Some(squash("No, exit").as_str());
        if answers.standing(rule, &identity, Instant::now()) && !partially_applied {
            trace(
                paths,
                run,
                pane,
                stage,
                status,
                Some(&screen),
                &format!("answered:{rule}"),
                false,
            );
            return Ok(Screen::Answered { rule });
        }
        // Herdr detects this native worker's kind, so a missing status is the
        // startup frame before it does. Keys pressed into that frame moved the
        // cursor and lost the Enter, so wait for the status.
        if !herdr_has_reported(status, run) {
            trace(
                paths,
                run,
                pane,
                stage,
                status,
                Some(&screen),
                "waiting-for-herdr",
                false,
            );
            return Ok(Screen::Clear);
        }
        let mut call = vec!["pane", "send-keys", pane];
        call.extend(keys.iter().copied());
        if let Err(failure) = herdr.call(&call) {
            trace(
                paths,
                run,
                pane,
                stage,
                status,
                Some(&screen),
                &format!("error:{rule}"),
                false,
            );
            return Err(anyhow::anyhow!(
                "brgr could not answer the {rule} screen: {failure}"
            ));
        }
        eprintln!("brgr pane mode · {rule}: pressed {}", keys.join(" "));
        record(&log_path(paths, run), pane, rule, keys);
        trace(
            paths,
            run,
            pane,
            stage,
            status,
            Some(&screen),
            &format!("resolve:{rule}"),
            true,
        );
        std::thread::sleep(REDRAW);
        answers.note_answer(rule, identity, selection, Instant::now());
    } else {
        // A readable screen that is no longer a dialog invalidates the
        // answer; an unreadable one returned above and kept it.
        answers.forget();
        let label = match &outcome {
            Screen::Unknown(_) => "unknown",
            _ => "clear",
        };
        trace(paths, run, pane, stage, status, Some(&screen), label, false);
    }
    Ok(outcome)
}

/// Whether Herdr has looked at this worker's agent yet. A native worker whose
/// kind Herdr detects has no status in the startup frame, and a rule pressed
/// then moves the cursor while losing its Enter.
fn herdr_has_reported(status: Option<&str>, run: &PaneRunArgs) -> bool {
    status.is_some() || run.native_executable.is_none() || !herdr_detects(&run.kind)
}

/// The kinds Herdr's lifecycle detection covers. `gjc`, `command-code` and the
/// test `fixture` run in brgr's native host under a kind Herdr does not know,
/// so they never report a status.
fn herdr_detects(kind: &str) -> bool {
    !matches!(kind, "gjc" | "command-code" | "fixture")
}

/// The workspace-trust choice the cursor points at, squashed, so a later poll
/// can tell whether only the cursor moved. `None` when no cursor is drawn.
fn trust_selection(screen: &str) -> Option<String> {
    let options = menu_options(screen);
    if let Some(option) = options.iter().find(|option| option.selected) {
        return Some(squash(&option.text));
    }
    let flat = squash(screen);
    ["Yes, I trust this folder", "No, exit"]
        .into_iter()
        .find(|label| {
            ["❯", "›", ">"]
                .iter()
                .any(|cursor| flat.contains(&format!("{cursor}{}", squash(label))))
        })
        .map(squash)
}

/// What makes a dialog the same one for the hold. Every rule but
/// `workspace-trust` compares the exact text, so any changed screen is a new
/// dialog. `workspace-trust` compares what identifies the prompt itself (the
/// validated workspace and its trust choices, in order), so an elapsed counter,
/// a spinner, a redrawn cursor, or refolded text does not answer it twice.
fn dialog_identity(rule: &str, screen: &str, workspace: &Path) -> String {
    if rule != "workspace-trust" {
        return format!("{rule}\n{screen}");
    }
    let options = menu_options(screen);
    let labels: Vec<String> = if options.is_empty() {
        ["Yes, I trust this folder", "No, exit"]
            .into_iter()
            .filter(|label| squash(screen).contains(&squash(label)))
            .map(squash)
            .collect()
    } else {
        options.iter().map(|option| squash(&option.text)).collect()
    };
    format!(
        "workspace-trust\n{}\n{}",
        squash(&workspace.to_string_lossy()),
        labels.join("\u{1f}")
    )
}

/// How long a new pane's shell may take to draw anything.
const SHELL_DRAW: Duration = Duration::from_secs(5);
const SHELL_DRAW_POLL: Duration = Duration::from_millis(100);

fn is_shell_update_offer(screen: &str) -> bool {
    let lower = screen.to_lowercase();
    lower.contains("[oh-my-zsh]") && lower.contains("would you like to update?")
}

/// Waits for a new pane's shell to stop drawing before anything is typed into it.
/// A shell startup question takes the next key as its answer: oh-my-zsh's update
/// offer ate the first letter of the command, seen live as `zsh: command not
/// found: odex`, on every pane until the offer was answered.
pub(super) fn settle_shell(herdr: &Herdr, pane: &str, run: &PaneRunArgs, paths: &Paths) {
    let started = Instant::now();
    let mut answered = false;
    let mut last = String::new();
    let mut unchanged = 0;
    while started.elapsed() < SHELL_DRAW {
        let Some(screen) = herdr.screen(pane) else {
            return;
        };
        if !answered && is_shell_update_offer(&screen) {
            answered = true;
            let keys = ["n"];
            match herdr.call(&["pane", "send-keys", pane, keys[0]]) {
                Ok(_) => record(&log_path(paths, run), pane, "shell-update-offer", &keys),
                Err(failure) => {
                    eprintln!("brgr pane mode · could not skip a shell update offer: {failure}");
                }
            }
        } else if screen == last && !screen.trim().is_empty() {
            unchanged += 1;
            if unchanged >= 2 {
                return;
            }
        } else {
            unchanged = 0;
        }
        last = screen;
        std::thread::sleep(SHELL_DRAW_POLL);
    }
}

/// The per-attempt record of every key brgr pressed on its own.
pub(super) fn log_path(paths: &Paths, run: &PaneRunArgs) -> PathBuf {
    paths
        .runs
        .join(format!("{}-r{}.screens.log", run.task, run.revision))
}

fn record(path: &Path, pane: &str, rule: &str, keys: &[&str]) {
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let written = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{at} {pane} {rule} {}", keys.join(" ")));
    if let Err(error) = written {
        eprintln!("brgr pane mode · could not log the {rule} key press: {error}");
    }
}

/// The opt-in per-resolve trace. `BRGR_SCREEN_TRACE=1` appends one line per
/// call with the stage, observed Herdr status, a digest of the screen and what
/// resolve did. The screen text itself is never written.
#[allow(clippy::too_many_arguments)]
fn trace(
    paths: &Paths,
    run: &PaneRunArgs,
    pane: &str,
    stage: &str,
    status: Option<&str>,
    screen: Option<&str>,
    outcome: &str,
    keys: bool,
) {
    if std::env::var("BRGR_SCREEN_TRACE").as_deref() != Ok("1") {
        return;
    }
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let digest = screen.map_or_else(|| "unreadable".to_owned(), screen_digest);
    let line = trace_line(
        at,
        pane,
        stage,
        status.unwrap_or("none"),
        &digest,
        outcome,
        keys,
    );
    let written = OpenOptions::new()
        .create(true)
        .append(true)
        .open(trace_path(paths, run))
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(error) = written {
        eprintln!("brgr pane mode · could not write the screen trace: {error}");
    }
}

fn trace_line(
    at: u128,
    pane: &str,
    stage: &str,
    status: &str,
    digest: &str,
    outcome: &str,
    keys: bool,
) -> String {
    let keys = if keys { "sent" } else { "none" };
    format!(
        "{at} pane={pane} stage={stage} status={status} sha256={digest} outcome={outcome} keys={keys}"
    )
}

fn trace_path(paths: &Paths, run: &PaneRunArgs) -> PathBuf {
    paths
        .runs
        .join(format!("{}-r{}.screens.trace", run.task, run.revision))
}

fn screen_digest(screen: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(screen.as_bytes()) {
        let _ = write!(digest, "{byte:02x}");
    }
    digest
}

/// The dialog a rule last answered in this pane and run.
struct Answer {
    rule: &'static str,
    /// What makes a later screen the same dialog: the exact text for every
    /// rule but `workspace-trust`, which uses its stable identity.
    identity: String,
    /// The `workspace-trust` choice under the cursor at the press, so a later
    /// poll can tell that only the cursor key was applied. `None` for other
    /// rules and when no cursor was drawn.
    selection: Option<String>,
    at: Instant, // taken after the REDRAW sleep
}

/// A pane/run's memory of the dialog a rule answered. It is shared by every
/// stage that resolves a screen so a handoff does not press the same dialog
/// twice; it is never copied or dropped at a stage boundary.
pub(super) struct AnswerMemory {
    answer: Option<Answer>,
}

impl AnswerMemory {
    pub(super) fn new() -> Self {
        Self { answer: None }
    }

    /// True while `identity` is the dialog `rule` answered less than
    /// [`REPRESS_AFTER`] ago.
    fn standing(&self, rule: &str, identity: &str, now: Instant) -> bool {
        self.answer.as_ref().is_some_and(|answer| {
            answer.rule == rule
                && answer.identity == identity
                && now.saturating_duration_since(answer.at) < REPRESS_AFTER
        })
    }

    /// The `workspace-trust` choice under the cursor at the last press.
    fn selection(&self) -> Option<&str> {
        self.answer
            .as_ref()
            .and_then(|answer| answer.selection.as_deref())
    }

    fn note_answer(
        &mut self,
        rule: &'static str,
        identity: String,
        selection: Option<String>,
        at: Instant,
    ) {
        self.answer = Some(Answer {
            rule,
            identity,
            selection,
            at,
        });
    }

    /// Forgets the answered dialog because it changed or the agent began
    /// working. Timers and press budgets live in [`ScreenWatch`], not here.
    pub(super) fn forget(&mut self) {
        self.answer = None;
    }
}

/// Bounds how long a screen no rule covers may stand and counts presses.
pub(super) struct ScreenWatch {
    kind: String,
    limit: Duration,
    since: Option<Instant>,
    presses: Vec<(&'static str, u32)>,
}

/// How many times one rule may answer the same standing screen before the run
/// fails: a screen a key does not clear would otherwise be pressed forever.
const PRESS_BUDGET: u32 = 5;

impl ScreenWatch {
    pub(super) fn new(kind: &str) -> Self {
        Self::with_limit(kind, screen_deadline())
    }

    pub(super) fn with_limit(kind: &str, limit: Duration) -> Self {
        Self {
            kind: kind.to_owned(),
            limit,
            since: None,
            presses: Vec::new(),
        }
    }

    /// Records a key press for `rule`. Fails when it keeps being needed.
    pub(super) fn pressed(&mut self, rule: &'static str) -> Result<()> {
        let count =
            if let Some((_, count)) = self.presses.iter_mut().find(|(name, _)| *name == rule) {
                *count += 1;
                *count
            } else {
                self.presses.push((rule, 1));
                1
            };
        if count > PRESS_BUDGET {
            bail!(
                "the {} agent kept showing a screen brgr answers ({rule}); {PRESS_BUDGET} presses did not clear it",
                self.kind
            );
        }
        Ok(())
    }

    /// Records a poll. Fails once an unknown screen has stood past the limit.
    pub(super) fn observe(&mut self, screen: &Screen) -> Result<()> {
        match screen {
            Screen::Unknown(text) => self.stuck(text),
            Screen::Clear => {
                self.since = None;
                self.presses.clear();
                Ok(())
            }
            Screen::Resolve { .. } | Screen::Answered { .. } => {
                self.since = None;
                Ok(())
            }
        }
    }

    /// Records a poll where Herdr reports `blocked` but no rule answered.
    pub(super) fn observe_blocked(&mut self, screen: Option<&str>) -> Result<()> {
        self.stuck(&screen.map(head).unwrap_or_default())
    }

    pub(super) fn reset(&mut self) {
        self.since = None;
        self.presses.clear();
    }

    /// Stops the unknown-screen clock but keeps the press counts, for an
    /// agent whose screen moved without Herdr saying it works.
    pub(super) fn restart_clock(&mut self) {
        self.since = None;
    }

    fn stuck(&mut self, text: &str) -> Result<()> {
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= self.limit {
            bail!(
                "the {} agent stayed on a screen no brgr rule answers for {}s: {text}",
                self.kind,
                self.limit.as_secs()
            );
        }
        Ok(())
    }
}

struct MenuOption {
    selected: bool,
    text: String,
}

/// The numbered options on screen, in order. `selected` marks the one a
/// cursor glyph points at.
fn menu_options(screen: &str) -> Vec<MenuOption> {
    screen
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_matches(['│', '┃', '║']).trim();
            let (selected, rest) = ["›", "❯", ">", "▸"]
                .iter()
                .find_map(|cursor| line.strip_prefix(cursor))
                .map_or((false, line), |rest| (true, rest.trim_start()));
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            let after = rest.get(digits..)?;
            (digits > 0 && (after.starts_with('.') || after.starts_with(')'))).then(|| MenuOption {
                selected,
                text: after[1..].trim().to_owned(),
            })
        })
        .collect()
}

fn is_skip(option: &str) -> bool {
    let option = option.to_lowercase();
    !option.contains("update now")
        && !option.contains("install")
        && [
            "skip",
            "not now",
            "later",
            "no thanks",
            "dismiss",
            "remind me",
        ]
        .iter()
        .any(|word| option.contains(word))
}

/// Marks the quoted screen in an error; nothing after it leaves the machine.
pub(crate) const LAST_SCREEN: &str = "last screen:";

/// The last rows of the screen, for an error message.
pub(super) fn tail(screen: &str) -> String {
    head(screen)
}

/// The last rows of the screen. A dialog sits at the bottom, below whatever
/// scrolled past (the shell line that launched the agent, for one).
fn head(screen: &str) -> String {
    let rows: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|row| !row.is_empty())
        .collect();
    rows[rows.len().saturating_sub(8)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The screen of the `OpenCode` pane that stalled task `38d33b4e` (D2 in the
    /// dogfood log), kept verbatim apart from trailing blank rows.
    const OPENCODE_UPDATED: &str = "\
                                                                                 ┃                               ┃
                                                                                 ┃  MCP Authentication Required  ┃
                                                                                 ┃                               ┃
                                                                                 ┃  Updating to v2.0.15…         ┃
                                                                                 ┃                               ┃

                              Update Complete                                      esc

                              Successfully updated to OpenCode v2.0.15. Please
                              restart the application.


                                                                                 ok

                     ┃
                     ┃  Ask anything… \"Fix broken tests\"
                     ┃
                     ┃  Build auto · Big Pickle OpenCode Zen
                     ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
                                                                     tab agents  ctrl+p commands
";

    fn work() -> &'static Path {
        Path::new("/repo/task")
    }

    #[test]
    fn an_applied_self_update_is_dismissed_with_esc_not_enter() {
        assert_eq!(
            classify(OPENCODE_UPDATED, work()),
            Screen::Resolve {
                rule: "update-applied",
                keys: vec!["esc"],
            }
        );
    }

    #[test]
    fn an_mcp_auth_toast_alone_does_not_block_the_run() {
        let toast = OPENCODE_UPDATED
            .lines()
            .filter(|line| !line.contains("Update Complete") && !line.contains("restart"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(classify(&toast, work()), Screen::Clear);
    }

    #[test]
    fn the_claude_trust_prompt_for_the_task_workspace_is_accepted() {
        let screen =
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["down", "enter"],
            }
        );
        let selected =
            "Accessing workspace:\n\n/repo/task\n\n  No, exit\n❯ Yes, I trust this folder\n";
        assert_eq!(
            classify(selected, work()),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["enter"],
            }
        );
    }

    /// Verbatim from a real Codex 0.160 pane (dogfood D13), path wrapped as shown.
    const CODEX_TRUST: &str = "\
  Folder access
  /private/tmp/scratchpad/cod
  ex owner repo
  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings.
  Folder settings can run code automatically, even without a model request. Continue only if you
  trust these files. Your trust decision will be saved.
› 1. Trust and continue
  2. Back to Agent Command Center
  enter continue · esc back
";

    #[test]
    fn the_codex_trust_prompt_for_the_task_workspace_is_accepted() {
        assert_eq!(
            classify(
                CODEX_TRUST,
                Path::new("/private/tmp/scratchpad/codex owner repo")
            ),
            Screen::Resolve {
                rule: "codex-trust",
                keys: vec!["enter"],
            }
        );
        // The cursor on the other option still ends on "Trust and continue".
        let moved = CODEX_TRUST
            .replace("› 1. Trust and continue", "  1. Trust and continue")
            .replace("  2. Back", "› 2. Back");
        assert_eq!(
            classify(
                &moved,
                Path::new("/private/tmp/scratchpad/codex owner repo")
            ),
            Screen::Resolve {
                rule: "codex-trust",
                keys: vec!["up", "enter"],
            }
        );
    }

    #[test]
    fn the_codex_trust_prompt_for_another_folder_is_not_accepted() {
        assert!(matches!(
            classify(CODEX_TRUST, Path::new("/elsewhere")),
            Screen::Unknown(_)
        ));
    }

    #[test]
    fn a_trust_prompt_for_another_folder_is_not_accepted() {
        let screen =
            "Accessing workspace:\n\n/elsewhere\n\n❯ No, exit\n  Yes, I trust this folder\n";
        assert!(matches!(classify(screen, work()), Screen::Unknown(_)));
    }

    /// Synthetic: modelled on the Codex update offer described in v2.9.3.
    #[test]
    fn an_update_offer_is_skipped_by_moving_to_the_skip_option() {
        let screen = "\
 Update available! 0.1.0 -> 0.2.0

 › 1. Update now
   2. Skip
   3. Skip until next version
";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Synthetic.
    #[test]
    fn an_update_offer_with_the_cursor_on_skip_only_presses_enter() {
        let screen = "Update available\n  1. Update now\n❯ 2. Not now\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["enter"],
            }
        );
    }

    /// Captured from Claude Code 2.1.318 on its first run in a Herdr pane; the
    /// cursor starts on "Yes, try it".
    const CLAUDE_RENDERER_OFFER: &str =
        include_str!("../../../../testdata/fixtures/screens/claude-2.1.318-renderer-offer.txt");

    #[test]
    fn claudes_renderer_offer_is_declined_by_moving_to_not_now() {
        assert_eq!(
            classify(CLAUDE_RENDERER_OFFER, Path::new("/somewhere")),
            Screen::Resolve {
                rule: "renderer-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
    }

    #[test]
    fn a_question_about_a_fullscreen_renderer_without_that_offer_is_not_answered() {
        let screen = "Should the fullscreen renderer be the default?\n❯ 1. Yes\n  2. Not now\n";
        assert!(matches!(
            classify(screen, Path::new("/somewhere")),
            Screen::Unknown(_)
        ));
    }

    /// Captured from an `OpenCode` worker pane on 2026-11-17 (issue #416).
    const OPENCODE_WEB_SEARCH: &str = "\
  ┃  Web Search
  ┃
  ┃  Allow OpenCode to search the web for up-to-date information?
  ┃
  ┃  1. Allow search via Exa, Firecrawl, Parallel, Tavily, TinyFish
  ┃  2. Choose another provider
  ┃  3. Disable web search
  ┃
  ┃  ↑↓ select  enter submit  esc dismiss
";

    #[test]
    fn opencodes_web_search_prompt_is_dismissed_with_esc() {
        assert_eq!(
            classify(OPENCODE_WEB_SEARCH, work()),
            Screen::Resolve {
                rule: "opencode-web-search-dismiss",
                keys: vec!["esc"],
            }
        );
    }

    #[test]
    fn opencodes_web_search_prompt_never_presses_allow_from_any_row() {
        let screens = [
            OPENCODE_WEB_SEARCH.to_owned(),
            OPENCODE_WEB_SEARCH.replace("┃  1. Allow", "┃❯ 1. Allow"),
            OPENCODE_WEB_SEARCH.replace("┃  2. Choose", "┃❯ 2. Choose"),
            OPENCODE_WEB_SEARCH.replace("┃  3. Disable", "┃❯ 3. Disable"),
        ];
        for screen in screens {
            let Screen::Resolve { rule, keys } = classify(&screen, work()) else {
                panic!("expected a dismiss, got {:?}", classify(&screen, work()));
            };
            assert_eq!(rule, "opencode-web-search-dismiss", "{screen}");
            assert_eq!(keys, vec!["esc"], "{screen}");
        }
    }

    /// Captured from Claude Code 2.1.325 on 2026-11-17 (issue #416).
    const CLAUDE_LSP_PLUGIN: &str = "\
 LSP plugin recommendation

   LSP provides code intelligence like go-to-definition and error checking

   Plugin: rust-analyzer-lsp
   Marketplace: claude-plugins-official
   Triggered by: .rs files
   Description: Rust language server for code intelligence and analysis

   Plugins can run code on your computer, with access to your files and shell.
   Would you like to install this LSP plugin?
   ❯ 1. Yes, install
     2. No, not now
     3. Never for this plugin
     4. Disable all LSP recommendations
";

    #[test]
    fn claudes_lsp_plugin_recommendation_is_declined_with_not_now() {
        assert_eq!(
            classify(CLAUDE_LSP_PLUGIN, work()),
            Screen::Resolve {
                rule: "lsp-plugin-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
    }

    #[test]
    fn claudes_lsp_plugin_recommendation_never_presses_yes_install_from_any_row() {
        let screens = [
            (CLAUDE_LSP_PLUGIN.to_owned(), vec!["down", "enter"]),
            (
                CLAUDE_LSP_PLUGIN
                    .replace("❯ 1. Yes, install", "  1. Yes, install")
                    .replace("  2. No, not now", "❯ 2. No, not now"),
                vec!["enter"],
            ),
            (
                CLAUDE_LSP_PLUGIN
                    .replace("❯ 1. Yes, install", "  1. Yes, install")
                    .replace("  3. Never for this plugin", "❯ 3. Never for this plugin"),
                vec!["up", "enter"],
            ),
            (
                CLAUDE_LSP_PLUGIN
                    .replace("❯ 1. Yes, install", "  1. Yes, install")
                    .replace(
                        "  4. Disable all LSP recommendations",
                        "❯ 4. Disable all LSP recommendations",
                    ),
                vec!["up", "up", "enter"],
            ),
        ];
        for (screen, expected) in screens {
            assert_eq!(
                classify(&screen, work()),
                Screen::Resolve {
                    rule: "lsp-plugin-offer-skip",
                    keys: expected,
                },
                "{screen}"
            );
        }
    }

    /// Captured from Codex CLI 0.166.0 on its first run after
    /// `brgr integrate codex install`. Text sent to this screen is lost.
    const CODEX_HOOKS_REVIEW: &str =
        include_str!("../../../../testdata/fixtures/screens/codex-0.166-hooks-review.txt");

    #[test]
    fn codexs_hook_review_is_answered_by_trusting_all() {
        assert_eq!(
            classify(CODEX_HOOKS_REVIEW, Path::new("/somewhere")),
            Screen::Resolve {
                rule: "codex-hooks-trust",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Captured from GitHub Copilot CLI 1.0.86 in a Herdr pane.
    const COPILOT_TRUST: &str =
        include_str!("../../../../testdata/fixtures/screens/copilot-1.0.86-trust.txt");
    const COPILOT_WORKSPACE: &str = "/private/tmp/claude-501/-Users-justn-dev-brgr/b54b7525-6101-4186-a2b9-0e32355d043a/scratchpad/cap-copilot-1791247661";

    /// Captured from Codex CLI 0.160 started in a git worktree.
    const CODEX_WORKTREE_TRUST: &str =
        include_str!("../../../../testdata/fixtures/screens/codex-0.160-worktree-trust.txt");
    const CODEX_WORKTREE: &str = "/private/tmp/claude-501/-Users-justn-dev-brgr/b54b7525-6101-4186-a2b9-0e32355d043a/scratchpad/cap-codex-wt";
    const CODEX_REPOSITORY: &str = "/private/tmp/claude-501/-Users-justn-dev-brgr/b54b7525-6101-4186-a2b9-0e32355d043a/scratchpad/cap-codex-repo";

    #[test]
    fn codex_trust_in_a_task_worktree_names_and_accepts_its_repository() {
        assert_eq!(
            codex_trust(CODEX_WORKTREE_TRUST, Path::new(CODEX_WORKTREE), || {
                Some(PathBuf::from(CODEX_REPOSITORY))
            }),
            Some(Screen::Resolve {
                rule: "codex-trust",
                keys: vec!["enter"],
            })
        );
    }

    /// Captured from Codex CLI 0.166.0 in a task worktree: the worktree path has a
    /// space in it, the root is on two lines, and the second choice is
    /// `Back to Agent Command Center` (0.160 said `Quit`).
    const CODEX_0166_WORKTREE_TRUST: &str =
        include_str!("../../../../testdata/fixtures/screens/codex-0.166-worktree-trust.txt");
    const CODEX_0166_WORKTREE: &str =
        "/Users/justn/Library/Application Support/brgr/worktrees/ws/6ca4cbb7";
    const CODEX_0166_REPOSITORY: &str = "/private/tmp/claude-501/-Users-justn-dev-brgr/2c6e674e-b811-4b53-8577-a4d60a8cc2ab/scratchpad/m226/ws";

    #[test]
    fn codex_0166_trust_in_a_task_worktree_names_and_accepts_its_repository() {
        assert_eq!(
            codex_trust(
                CODEX_0166_WORKTREE_TRUST,
                Path::new(CODEX_0166_WORKTREE),
                || Some(PathBuf::from(CODEX_0166_REPOSITORY))
            ),
            Some(Screen::Resolve {
                rule: "codex-trust",
                keys: vec!["enter"],
            })
        );
    }

    #[test]
    fn codex_trust_for_a_root_that_is_not_the_worktrees_repository_is_refused() {
        for primary in [None, Some("/private/tmp/other-repo"), Some(CODEX_WORKTREE)] {
            assert!(
                matches!(
                    codex_trust(CODEX_WORKTREE_TRUST, Path::new(CODEX_WORKTREE), || {
                        primary.map(PathBuf::from)
                    }),
                    Some(Screen::Unknown(_))
                ),
                "{primary:?}"
            );
        }
    }

    /// Captured from Cursor Agent 2026.10.22 in a wide Herdr pane while it waits
    /// for an answer; Cursor draws a `▶` before the selected `[a]` option.
    const CURSOR_TRUST: &str =
        include_str!("../../../../testdata/fixtures/screens/cursor-2026.10.22-trust-wide.txt");
    const CURSOR_WORKSPACE: &str = "/private/tmp/claude-501/-Users-justn-dev-brgr/2c6e674e-b811-4b53-8577-a4d60a8cc2ab/scratchpad/cur3-work";
    /// Captured from the same agent in a narrow pane after the prompt was
    /// answered: the box is still on screen and says it is trusting.
    const CURSOR_TRUST_ANSWERED: &str =
        include_str!("../../../../testdata/fixtures/screens/cursor-2026.10.22-trust.txt");
    const CURSOR_ANSWERED_WORKSPACE: &str = "/private/tmp/claude-501/-Users-justn-dev-brgr/2c6e674e-b811-4b53-8577-a4d60a8cc2ab/scratchpad/cursor-work";

    #[test]
    fn cursors_trust_prompt_for_the_task_workspace_is_accepted_with_its_key() {
        assert_eq!(
            classify(CURSOR_TRUST, Path::new(CURSOR_WORKSPACE)),
            Screen::Resolve {
                rule: "cursor-trust",
                keys: vec!["a"],
            }
        );
    }

    #[test]
    fn cursors_trust_prompt_for_another_folder_is_not_accepted() {
        for other in [
            "/private/tmp/claude-501/-Users-justn-dev-brgr/2c6e674e-b811-4b53-8577-a4d60a8cc2ab/scratchpad/cur3-wor",
            "/private/tmp/claude-501",
            "/somewhere/else",
        ] {
            assert!(
                matches!(classify(CURSOR_TRUST, Path::new(other)), Screen::Unknown(_)),
                "{other}"
            );
        }
    }

    #[test]
    fn cursors_answered_trust_box_still_on_screen_is_not_answered_again() {
        assert_eq!(
            classify(CURSOR_TRUST_ANSWERED, Path::new(CURSOR_ANSWERED_WORKSPACE)),
            Screen::Clear
        );
    }

    #[test]
    fn copilots_trust_prompt_for_the_task_workspace_is_accepted_once() {
        assert_eq!(
            classify(COPILOT_TRUST, Path::new(COPILOT_WORKSPACE)),
            Screen::Resolve {
                rule: "copilot-trust",
                keys: vec!["enter"],
            }
        );
    }

    #[test]
    fn copilots_trust_prompt_for_another_folder_is_not_accepted() {
        for other in [
            "/private/tmp/claude-501/-Users-justn-dev-brgr/b54b7525-6101-4186-a2b9-0e32355d043a/scratchpad/cap-copilot-179124766",
            "/private/tmp/claude-501",
            "/somewhere/else",
        ] {
            assert!(
                matches!(
                    classify(COPILOT_TRUST, Path::new(other)),
                    Screen::Unknown(_)
                ),
                "{other}"
            );
        }
    }

    #[test]
    fn copilots_idle_screen_needs_no_answer() {
        let idle = include_str!("../../../../testdata/fixtures/screens/copilot-1.0.86-idle.txt");
        assert_eq!(classify(idle, Path::new(COPILOT_WORKSPACE)), Screen::Clear);
    }

    /// Synthetic.
    #[test]
    fn an_agent_question_about_updating_something_is_not_an_update_offer() {
        for screen in [
            "Update the schema now?\n› 1. Yes\n  2. Later\n",
            "Upgrade the dependencies too?\n› 1. Yes\n  2. Skip\n",
            "> 1. update the config, then skip later steps\n  2. next\n",
        ] {
            assert!(
                matches!(classify(screen, work()), Screen::Unknown(_)),
                "{screen}"
            );
        }
    }

    #[test]
    fn an_update_offer_is_known_by_its_wording_or_version() {
        assert!(is_update_offer("a new version of codex is ready, update?"));
        assert!(is_update_offer("upgrade to v2.0.15"));
        assert!(!is_update_offer("update the 2.0 schema"));
        assert!(!is_update_offer("update readme.md and 1.2 notes"));
    }

    #[test]
    fn an_update_menu_without_a_skip_option_is_not_guessed() {
        let screen = "Update available\n› 1. Update now\n  2. Show release notes\n";
        assert!(matches!(classify(screen, work()), Screen::Unknown(_)));
    }

    /// Synthetic: Claude Code's documented Bypass Permissions warning, not
    /// captured from a pane here.
    #[test]
    fn the_bypass_permissions_warning_is_accepted() {
        let screen = "WARNING: Claude Code running in Bypass Permissions mode\n\nIn Bypass Permissions mode, Claude Code will not ask for your approval.\n\n❯ 1. No, exit\n  2. Yes, I accept\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "bypass-warning",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Synthetic: Claude Code's documented new-MCP-server prompt.
    #[test]
    fn a_new_mcp_server_prompt_is_trusted() {
        let screen = "New MCP server found in .mcp.json: docs\n\n❯ 1. Use this and all future MCP servers in this project\n  2. Use this MCP server\n  3. Continue without using this MCP server\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "mcp-trust",
                keys: vec!["down", "enter"],
            }
        );
    }

    #[test]
    fn a_numbered_claude_trust_menu_is_answered_by_moving_to_yes() {
        let screen = "Accessing workspace:\n\n/repo/task\n\nQuick safety check\n\n❯ 1. Yes, I trust this folder\n  2. No, exit\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["enter"],
            }
        );
        let other = screen
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("  2. No, exit", "❯ 2. No, exit");
        assert_eq!(
            classify(&other, work()),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["up", "enter"],
            }
        );
    }

    #[test]
    fn the_codex_trust_prompt_for_a_longer_path_is_not_accepted() {
        for longer in [
            "/private/tmp/scratchpad/codex owner repo-evil",
            "/private/tmp/scratchpad/codex owner repo/sub",
        ] {
            let screen =
                CODEX_TRUST.replace("/private/tmp/scratchpad/cod\n  ex owner repo", longer);
            assert!(
                matches!(
                    classify(
                        &screen,
                        Path::new("/private/tmp/scratchpad/codex owner repo")
                    ),
                    Screen::Unknown(_)
                ),
                "{longer}"
            );
        }
    }

    #[test]
    fn agent_prose_about_an_update_is_not_the_applied_update_dialog() {
        let prose = "The build finished. Update complete, please restart the dev server.\n";
        assert_eq!(classify(prose, work()), Screen::Clear);
    }

    #[test]
    fn sentence_followed_by_a_continue_notice_is_dismissed_with_enter() {
        assert_eq!(
            classify("│ • Startup complete. Press Enter to continue… │\n", work()),
            Screen::Resolve {
                rule: "continue-notice",
                keys: vec!["enter"],
            }
        );
    }

    #[test]
    fn prose_quoting_or_discussing_a_continue_notice_is_left_alone() {
        for prose in [
            "The tool prints \"press enter to continue\" when it stops.\n",
            "I saw press enter to continue in the log.\n",
            "Then it says press enter to continue.\n",
            "Press Enter to continue or Esc to cancel\n",
        ] {
            assert_eq!(classify(prose, work()), Screen::Clear, "{prose:?}");
        }
    }

    #[test]
    fn a_screen_that_a_key_does_not_clear_fails_the_run_after_the_budget() {
        let mut watch = ScreenWatch::new("claude");
        for _ in 0..PRESS_BUDGET {
            watch.pressed("continue-notice").unwrap();
        }
        let error = watch.pressed("continue-notice").unwrap_err().to_string();
        assert!(
            error.contains("continue-notice") && error.contains("did not clear"),
            "{error}"
        );
        // A clear screen is progress: the count starts over.
        watch.observe(&Screen::Clear).unwrap();
        watch.pressed("continue-notice").unwrap();
    }

    #[test]
    fn a_new_mcp_server_prompt_prefers_the_single_server_choice() {
        let screen = "New MCP server found in .mcp.json: docs\n\n❯ 1. Use this and all future MCP servers in this project\n  2. Use this MCP server\n  3. Continue without using this MCP server\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "mcp-trust",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Synthetic: a stray notice line must not turn an update menu into Enter.
    #[test]
    fn an_update_menu_with_a_continue_line_is_skipped_not_confirmed() {
        let screen = "Update available\n› 1. Update now\n  2. Skip\nPress Enter to continue\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Synthetic.
    #[test]
    fn a_continue_notice_is_dismissed_with_enter() {
        assert_eq!(
            classify(
                "Welcome to the new release\n\n  Press Enter to continue\n",
                work()
            ),
            Screen::Resolve {
                rule: "continue-notice",
                keys: vec!["enter"],
            }
        );
    }

    /// Synthetic.
    #[test]
    fn a_menu_no_rule_covers_is_unknown_and_never_pressed() {
        let screen = "Choose a theme\n› 1. Dark\n  2. Light\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Unknown("Choose a theme | › 1. Dark | 2. Light".to_owned())
        );
    }

    #[test]
    fn an_ordinary_agent_screen_is_clear() {
        assert_eq!(classify("some output\n❯ \n", work()), Screen::Clear);
    }

    fn fake_herdr(screen: &str) -> (tempfile::TempDir, Herdr) {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("herdr");
        std::fs::write(temp.path().join("screen"), screen).unwrap();
        crate::fast_script::write_fast_script(
            &binary,
            r#"#!/bin/sh
d=$(/usr/bin/dirname "$0")
case "$1 $2" in
 'pane read') /bin/cat "$d/screen";;
 'pane send-keys') echo "$@" >> "$d/keys";;
 *) exit 8;;
esac
"#,
        );
        let herdr = Herdr {
            binary: binary.into_os_string(),
            session: None,
        };
        (temp, herdr)
    }

    fn run_args(temp: &tempfile::TempDir) -> (Paths, PaneRunArgs) {
        let paths = Paths::new(Some(temp.path().join("home"))).unwrap();
        let run = PaneRunArgs {
            caller: None,
            native_executable: None,
            opencode_model: None,
            prompt_file: PathBuf::new(),
            workspace: PathBuf::from("/repo/task"),
            task: brgr_protocol::TaskId::new(),
            revision: 1,
            kind: "opencode".to_owned(),
            agent_args: Vec::new(),
            keep_pane: false,
        };
        (paths, run)
    }

    #[test]
    fn a_shell_update_offer_is_declined_before_anything_is_typed() {
        let (temp, herdr) = fake_herdr("[oh-my-zsh] Would you like to update? [Y/n] ");
        let (paths, run) = run_args(&temp);
        settle_shell(&herdr, "w1:p2", &run, &paths);
        let keys = std::fs::read_to_string(temp.path().join("keys")).unwrap();
        assert_eq!(keys.trim(), "pane send-keys w1:p2 n");
        let log = std::fs::read_to_string(log_path(&paths, &run)).unwrap();
        assert!(
            log.trim_end().ends_with("w1:p2 shell-update-offer n"),
            "{log}"
        );
    }

    #[test]
    fn a_plain_shell_prompt_presses_nothing() {
        let (temp, herdr) = fake_herdr("repo on main\n❯ update the docs? [Y/n]");
        let (paths, run) = run_args(&temp);
        settle_shell(&herdr, "w1:p2", &run, &paths);
        assert!(!temp.path().join("keys").exists());
    }

    #[test]
    fn resolve_presses_the_rule_keys_on_the_pane() {
        let (temp, herdr) = fake_herdr(OPENCODE_UPDATED);
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        let outcome = resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        assert!(matches!(outcome, Screen::Resolve { .. }));
        let keys = std::fs::read_to_string(temp.path().join("keys")).unwrap();
        assert_eq!(keys.trim(), "pane send-keys w1:p2 esc");
        let log = std::fs::read_to_string(log_path(&paths, &run)).unwrap();
        assert!(
            log.trim_end().ends_with("w1:p2 update-applied esc"),
            "{log}"
        );
    }

    #[test]
    fn resolve_presses_nothing_while_the_agent_is_working() {
        let screen = "agent output\n› 1. Update now\n  2. Skip\n";
        let (temp, herdr) = fake_herdr(screen);
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("working"),
                &mut answers,
                "test"
            )
            .unwrap(),
            Screen::Clear
        );
        assert!(!temp.path().join("keys").exists());
        assert!(!log_path(&paths, &run).exists());
    }

    #[test]
    fn opencodes_web_search_prompt_is_not_answered_while_working() {
        let (temp, herdr) = fake_herdr(OPENCODE_WEB_SEARCH);
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("working"),
                &mut answers,
                "test"
            )
            .unwrap(),
            Screen::Clear
        );
        assert!(!temp.path().join("keys").exists());
    }

    #[test]
    fn claudes_lsp_plugin_recommendation_is_not_answered_while_working() {
        let (temp, herdr) = fake_herdr(CLAUDE_LSP_PLUGIN);
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("working"),
                &mut answers,
                "test"
            )
            .unwrap(),
            Screen::Clear
        );
        assert!(!temp.path().join("keys").exists());
    }

    #[test]
    fn a_standing_answered_screen_is_not_answered_again() {
        let screen =
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n";
        let (temp, herdr) = fake_herdr(screen);
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        let second = resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        assert_eq!(
            second,
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        let keys = std::fs::read_to_string(temp.path().join("keys")).unwrap();
        assert_eq!(keys.trim(), "pane send-keys w1:p2 down enter");
        let log = std::fs::read_to_string(log_path(&paths, &run)).unwrap();
        assert_eq!(log.lines().count(), 1);
    }

    #[test]
    fn a_changed_screen_under_another_rule_is_answered_again() {
        let (temp, herdr) = fake_herdr("Setup done.\nPress Enter to continue\n");
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("screen"),
            "Welcome to the new release.\nPress Enter to continue\n",
        )
        .unwrap();
        let second = resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        assert_eq!(
            second,
            Screen::Resolve {
                rule: "continue-notice",
                keys: vec!["enter"],
            }
        );
        let keys = std::fs::read_to_string(temp.path().join("keys")).unwrap();
        let lines: Vec<_> = keys.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with("enter"), "{keys}");
    }

    #[test]
    fn a_working_agent_lets_the_same_screen_be_answered_again() {
        let screen =
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n";
        let (temp, herdr) = fake_herdr(screen);
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        let middle = resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("working"),
            &mut answers,
            "test",
        )
        .unwrap();
        assert_eq!(middle, Screen::Clear);
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        let keys = std::fs::read_to_string(temp.path().join("keys")).unwrap();
        assert_eq!(keys.trim().lines().count(), 2);
    }

    #[test]
    fn an_answered_screen_is_pressed_again_only_after_the_hold() {
        let screen =
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n";
        let identity = dialog_identity("workspace-trust", screen, work());
        let mut answers = AnswerMemory::new();
        let t0 = Instant::now();
        answers.note_answer("workspace-trust", identity.clone(), None, t0);
        assert!(answers.standing(
            "workspace-trust",
            &identity,
            t0 + REPRESS_AFTER.checked_sub(Duration::from_millis(1)).unwrap()
        ));
        assert!(!answers.standing("workspace-trust", &identity, t0 + REPRESS_AFTER));
        assert!(!answers.standing("workspace-trust", "other", t0));
    }

    #[test]
    fn an_unknown_menu_after_an_answer_is_reported_not_pressed() {
        let screen =
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n";
        let (temp, herdr) = fake_herdr(screen);
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("screen"),
            "Choose a theme\n› 1. Dark\n  2. Light\n",
        )
        .unwrap();
        let second = resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        assert!(matches!(second, Screen::Unknown(_)));
        let keys = std::fs::read_to_string(temp.path().join("keys")).unwrap();
        assert_eq!(keys.trim().lines().count(), 1);
    }

    #[test]
    fn an_update_menu_after_an_answered_notice_is_skipped_not_confirmed() {
        let (temp, herdr) = fake_herdr("Setup done.\nPress Enter to continue\n");
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("screen"),
            "Update available\nPress Enter to continue\n› 1. Update now\n  2. Skip\n",
        )
        .unwrap();
        let second = resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "test",
        )
        .unwrap();
        assert_eq!(
            second,
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
        let keys = std::fs::read_to_string(temp.path().join("keys")).unwrap();
        let lines: Vec<_> = keys.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with("down enter"), "{keys}");
    }

    #[test]
    fn a_hold_does_not_spend_the_press_budget_or_start_the_deadline() {
        let mut watch = ScreenWatch::with_limit("claude", Duration::ZERO);
        watch
            .observe(&Screen::Answered {
                rule: "workspace-trust",
            })
            .unwrap();
        assert!(watch.since.is_none());
    }

    #[test]
    fn deadline_resets_do_not_forget_the_answer() {
        let screen =
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n";
        let identity = dialog_identity("workspace-trust", screen, work());
        let mut watch = ScreenWatch::with_limit("claude", Duration::from_hours(1));
        let mut answers = AnswerMemory::new();
        answers.note_answer("workspace-trust", identity.clone(), None, Instant::now());
        watch.observe(&Screen::Clear).unwrap();
        watch.reset();
        watch.restart_clock();
        assert!(answers.standing("workspace-trust", &identity, Instant::now()));
    }

    #[test]
    fn an_unknown_screen_fails_the_run_with_its_text_after_the_deadline() {
        let mut watch = ScreenWatch::with_limit("opencode", Duration::ZERO);
        let error = watch
            .observe(&Screen::Unknown("Choose a theme".to_owned()))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("opencode") && error.contains("Choose a theme"),
            "{error}"
        );
    }

    #[test]
    fn a_resolved_screen_resets_the_deadline() {
        let mut watch = ScreenWatch::with_limit("claude", Duration::from_hours(1));
        watch.observe(&Screen::Unknown("x".to_owned())).unwrap();
        watch.observe(&Screen::Clear).unwrap();
        assert!(watch.since.is_none());
    }

    #[test]
    fn an_answered_screen_resets_the_deadline() {
        let mut watch = ScreenWatch::with_limit("claude", Duration::from_hours(1));
        watch.observe(&Screen::Unknown("x".to_owned())).unwrap();
        watch
            .observe(&Screen::Resolve {
                rule: "continue-notice",
                keys: vec!["enter"],
            })
            .unwrap();
        assert!(watch.since.is_none());
    }

    #[test]
    fn the_unknown_screen_deadline_is_three_minutes() {
        assert_eq!(SCREEN_DEADLINE, Duration::from_mins(3));
    }

    /// Synthetic: an offer whose skip option is worded "Later" is still moved
    /// to and answered, never confirmed.
    #[test]
    fn an_update_offer_is_skipped_by_its_later_option() {
        let screen = "Update available\n› 1. Update now\n  2. Later\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
    }

    fn trust() -> &'static str {
        "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n"
    }

    fn stage_watch() -> ScreenWatch {
        ScreenWatch::with_limit("claude", Duration::from_hours(1))
    }

    fn send_keys(temp: &tempfile::TempDir) -> Vec<String> {
        std::fs::read_to_string(temp.path().join("keys"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// One answer value passes through startup, the first prompt and the drive
    /// loop while only the stage-local watches are recreated. This is the
    /// lifetime the pane/run memory exists to pin.
    #[test]
    fn startup_answer_memory_survives_stage_handoff() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        let mut startup = stage_watch();
        assert!(matches!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("idle"),
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Resolve { .. }
        ));
        startup.pressed("workspace-trust").unwrap();
        let mut prompt = stage_watch();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("idle"),
                &mut answers,
                "first-prompt",
            )
            .unwrap(),
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        let _ = &mut prompt;
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("blocked"),
                &mut answers,
                "drive-blocked",
            )
            .unwrap(),
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        assert_eq!(send_keys(&temp), ["pane send-keys w1:p2 down enter"]);
    }

    /// Sequence A: an elapsed/spinner row or trailing whitespace changes the
    /// bytes while the trust choice is unchanged. It is the same dialog.
    #[test]
    fn trust_decoration_change_does_not_repress_within_hold() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        assert!(matches!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("idle"),
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Resolve { .. }
        ));
        std::thread::sleep(Duration::from_secs(1));
        std::fs::write(
            temp.path().join("screen"),
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n  ⎿ elapsed 2s\n\n",
        )
        .unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("idle"),
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        assert_eq!(send_keys(&temp), ["pane send-keys w1:p2 down enter"]);
    }

    /// The first press moved the cursor to Yes and lost its Enter. The
    /// selection now differs from the one at the press, so the answer is
    /// finished with the one key the current selection needs, without waiting
    /// for the 30-second hold.
    #[test]
    fn a_trust_selection_moved_after_the_press_is_finished_with_enter() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "start-native",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("screen"),
            "Accessing workspace:\n\n/repo/task\n\n  No, exit\n❯ Yes, I trust this folder\n",
        )
        .unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("idle"),
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["enter"],
            }
        );
        assert_eq!(
            send_keys(&temp),
            [
                "pane send-keys w1:p2 down enter",
                "pane send-keys w1:p2 enter"
            ]
        );
        let log = std::fs::read_to_string(log_path(&paths, &run)).unwrap();
        assert_eq!(log.lines().count(), 2, "{log}");
        // The cursor now stands still, so the new press is held like any other.
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("idle"),
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        assert_eq!(send_keys(&temp).len(), 2);
    }

    /// The selection at the press is still where the cursor is: nothing was
    /// applied yet, so the answer is held for the full [`REPRESS_AFTER`].
    #[test]
    fn an_unchanged_standing_trust_dialog_is_held_for_the_repress_hold() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "start-native",
        )
        .unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("idle"),
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        assert_eq!(send_keys(&temp), ["pane send-keys w1:p2 down enter"]);
    }

    /// A selection that moved to "No, exit" is never pressed. The hold runs
    /// out and the rule then moves from wherever the cursor is.
    #[test]
    fn a_trust_selection_moved_to_no_exit_is_not_pressed() {
        let selected =
            "Accessing workspace:\n\n/repo/task\n\n  No, exit\n❯ Yes, I trust this folder\n";
        let (temp, herdr) = fake_herdr(selected);
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("idle"),
            &mut answers,
            "start-native",
        )
        .unwrap();
        std::fs::write(temp.path().join("screen"), trust()).unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("idle"),
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        assert_eq!(send_keys(&temp), ["pane send-keys w1:p2 enter"]);
    }

    fn native_run(kind: &str, temp: &tempfile::TempDir) -> (Paths, PaneRunArgs) {
        let (paths, mut run) = run_args(temp);
        run.kind = kind.to_owned();
        run.native_executable = Some(PathBuf::from(format!("/bin/{kind}")));
        (paths, run)
    }

    /// Herdr has not detected the agent yet, so its status is absent. A rule
    /// that pressed now moved the cursor and lost the Enter (seen live: a 30 s
    /// stall). It waits for a status instead.
    #[test]
    fn a_native_harness_herdr_detects_is_not_pressed_before_a_status() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = native_run("claude", &temp);
        let mut answers = AnswerMemory::new();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                None,
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Clear
        );
        assert!(
            !temp.path().join("keys").exists(),
            "nothing is pressed before Herdr reports the agent"
        );
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("blocked"),
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["down", "enter"],
            }
        );
        assert_eq!(send_keys(&temp), ["pane send-keys w1:p2 down enter"]);
    }

    /// A kind Herdr has no detector for (`gjc` runs in brgr's native host) has
    /// no status at any point, so its screens are still answered.
    #[test]
    fn an_unclassified_harness_is_pressed_with_no_herdr_status() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = native_run("gjc", &temp);
        let mut answers = AnswerMemory::new();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                None,
                &mut answers,
                "start-native",
            )
            .unwrap(),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["down", "enter"],
            }
        );
        assert_eq!(send_keys(&temp), ["pane send-keys w1:p2 down enter"]);
    }

    #[test]
    fn unreadable_screen_and_idle_deadline_reset_preserve_answer() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        let mut watch = stage_watch();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("blocked"),
            &mut answers,
            "drive-blocked",
        )
        .unwrap();
        std::fs::remove_file(temp.path().join("screen")).unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("blocked"),
                &mut answers,
                "drive-blocked",
            )
            .unwrap(),
            Screen::Clear
        );
        watch.reset(); // idle/done resets the stage timers only
        watch.restart_clock();
        std::fs::write(temp.path().join("screen"), trust()).unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("blocked"),
                &mut answers,
                "drive-blocked",
            )
            .unwrap(),
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        assert_eq!(send_keys(&temp).len(), 1);
    }

    #[test]
    fn working_never_presses_and_allows_a_later_trust_answer() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("blocked"),
            &mut answers,
            "drive-blocked",
        )
        .unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("working"),
                &mut answers,
                "drive-blocked",
            )
            .unwrap(),
            Screen::Clear
        );
        assert_eq!(
            send_keys(&temp).len(),
            1,
            "nothing is pressed while working"
        );
        // drive does not resolve while working, so it invalidates directly.
        answers.forget();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("blocked"),
            &mut answers,
            "drive-blocked",
        )
        .unwrap();
        assert_eq!(send_keys(&temp).len(), 2);
    }

    #[test]
    fn different_dialog_and_wrong_workspace_are_not_hidden_by_trust_hold() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("blocked"),
            &mut answers,
            "drive-blocked",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("screen"),
            "Choose a theme\n› 1. Dark\n  2. Light\n",
        )
        .unwrap();
        assert!(matches!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("blocked"),
                &mut answers,
                "drive-blocked",
            )
            .unwrap(),
            Screen::Unknown(_)
        ));
        std::fs::write(
            temp.path().join("screen"),
            "Accessing workspace:\n\n/elsewhere\n\n❯ No, exit\n  Yes, I trust this folder\n",
        )
        .unwrap();
        assert!(matches!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("blocked"),
                &mut answers,
                "drive-blocked",
            )
            .unwrap(),
            Screen::Unknown(_)
        ));
        std::fs::write(
            temp.path().join("screen"),
            "Update available\nPress Enter to continue\n› 1. Update now\n  2. Skip\n",
        )
        .unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("blocked"),
                &mut answers,
                "drive-blocked",
            )
            .unwrap(),
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
        let keys = send_keys(&temp);
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert_eq!(keys[1], "pane send-keys w1:p2 down enter", "{keys:?}");
    }

    #[test]
    fn standing_trust_represses_only_after_thirty_seconds() {
        let screen = trust();
        let identity = dialog_identity("workspace-trust", screen, work());
        let mut answers = AnswerMemory::new();
        let t0 = Instant::now();
        answers.note_answer("workspace-trust", identity.clone(), None, t0);
        let just_before = REPRESS_AFTER.checked_sub(Duration::from_millis(1)).unwrap();
        assert!(answers.standing("workspace-trust", &identity, t0 + just_before));
        assert!(!answers.standing("workspace-trust", &identity, t0 + REPRESS_AFTER));
    }

    #[test]
    fn a_cosmetic_trust_redraw_does_not_extend_the_hold() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, run) = run_args(&temp);
        let mut answers = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("blocked"),
            &mut answers,
            "drive-blocked",
        )
        .unwrap();
        let first_at = answers.answer.as_ref().unwrap().at;
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(
            temp.path().join("screen"),
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n  working… 1s\n",
        )
        .unwrap();
        assert_eq!(
            resolve(
                &herdr,
                "w1:p2",
                &run,
                &paths,
                Some("blocked"),
                &mut answers,
                "drive-blocked",
            )
            .unwrap(),
            Screen::Answered {
                rule: "workspace-trust"
            }
        );
        assert_eq!(answers.answer.as_ref().unwrap().at, first_at);
    }

    #[test]
    fn answer_memory_is_isolated_by_pane_and_run() {
        let (temp, herdr) = fake_herdr(trust());
        let (paths, mut run) = run_args(&temp);
        let mut one = AnswerMemory::new();
        let mut another = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("blocked"),
            &mut one,
            "drive-blocked",
        )
        .unwrap();
        resolve(
            &herdr,
            "w1:p3",
            &run,
            &paths,
            Some("blocked"),
            &mut another,
            "drive-blocked",
        )
        .unwrap();
        assert_eq!(send_keys(&temp).len(), 2, "each pane answers once");
        run.revision = 2;
        let mut next_revision = AnswerMemory::new();
        resolve(
            &herdr,
            "w1:p2",
            &run,
            &paths,
            Some("blocked"),
            &mut next_revision,
            "drive-blocked",
        )
        .unwrap();
        assert_eq!(
            send_keys(&temp).len(),
            3,
            "a new revision does not inherit it"
        );
    }

    #[test]
    fn screen_trace_line_carries_fields_never_screen_text() {
        let digest = screen_digest("Accessing workspace: secret text");
        assert_eq!(digest.len(), 64);
        let line = trace_line(
            1_700_000_000_123,
            "w1:p2",
            "drive-blocked",
            "blocked",
            &digest,
            "resolve:workspace-trust",
            true,
        );
        assert!(
            line.starts_with("1700000000123 pane=w1:p2 stage=drive-blocked status=blocked sha256="),
            "{line}"
        );
        assert!(
            line.contains("outcome=resolve:workspace-trust keys=sent"),
            "{line}"
        );
        assert!(
            !line.contains("Accessing") && !line.contains("secret"),
            "{line}"
        );
        assert!(
            trace_line(0, "p", "s", "idle", "unreadable", "clear", false).ends_with("keys=none")
        );
    }
}
