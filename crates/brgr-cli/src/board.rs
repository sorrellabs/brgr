//! Terminal-free task board model and text renderer.

use std::{collections::BTreeMap, path::Path};

use anyhow::Result;
use brgr_protocol::{OwnerId, TaskId};
use brgr_store::BoardStore;
use dialoguer::console::measure_text_width;

use crate::{Paths, aliases::Alias, config::Config, human};

pub(crate) const BOARD_LIMIT: usize = 20;

const PROJECT_CAP: usize = 20;
const HARNESS_CAP: usize = 17;
const ALIAS_CAP: usize = 10;
const MODEL_CAP: usize = 24;
const SHRINK_FLOOR: usize = 6;
const SESSION_ID_CHARS: usize = 8;
const TASK_ID_CHARS: usize = 8;

pub(crate) enum Scope {
    All,
    Session {
        session: String,
        owner: Option<OwnerId>,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum Host {
    Terminal,
    HerdrPlugin,
}

pub(crate) struct BoardRow {
    pub(crate) task_id: TaskId,
    pub(crate) project: String,
    pub(crate) harness: String,
    pub(crate) alias: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) revision: u32,
    pub(crate) state: String,
    pub(crate) outcome: String,
    pub(crate) verdict: String,
}

pub(crate) struct BoardModel {
    pub(crate) scope_line: String,
    pub(crate) rows: Vec<BoardRow>,
    pub(crate) notes: Vec<String>,
}

pub(crate) fn load(paths: &Paths, scope: &Scope) -> Result<BoardModel> {
    let store = BoardStore::open_existing(&paths.store)?;
    let rows = match scope {
        Scope::All => store.rows(BOARD_LIMIT)?,
        Scope::Session { session, owner } => {
            store.rows_for_session(session, owner.as_ref(), BOARD_LIMIT)?
        }
    };
    let (aliases, notes) = match Config::load(&paths.config) {
        Ok(config) => (config.aliases, Vec::new()),
        Err(_) => (
            BTreeMap::new(),
            vec!["aliases not shown: config.toml did not load".to_owned()],
        ),
    };
    let scope_line = match scope {
        Scope::All => "all owners".to_owned(),
        Scope::Session { session, .. } => {
            format!(
                "my tasks · session {}",
                human::cell(session, SESSION_ID_CHARS)
            )
        }
    };
    let rows = rows
        .into_iter()
        .map(|row| {
            let project = project_name(&row.workspace, row.checkout.as_deref());
            let alias = alias_for(
                &aliases,
                &row.harness_id,
                row.requested_model.as_deref(),
                row.requested_effort.as_deref(),
            )
            .map(ToOwned::to_owned);
            BoardRow {
                task_id: row.task_id,
                project,
                harness: row.harness_id,
                alias,
                model: row.requested_model,
                revision: row.revision,
                state: format!("{:?}", row.attempt_state).to_lowercase(),
                outcome: row.result_outcome.map_or_else(
                    || "-".to_owned(),
                    |value| format!("{value:?}").to_lowercase(),
                ),
                verdict: row.decision_verdict.map_or_else(
                    || "-".to_owned(),
                    |value| format!("{value:?}").to_lowercase(),
                ),
            }
        })
        .collect();
    Ok(BoardModel {
        scope_line,
        rows,
        notes,
    })
}

pub(crate) fn render(model: &BoardModel, host: Host, size: Option<(u16, u16)>) -> Vec<String> {
    let mut lines = vec![
        "brgr · managed tasks".to_owned(),
        human::safe_text(&model.scope_line),
        "The owner session decides each sealed result with brgr accept or brgr reject. This board never decides."
            .to_owned(),
    ];
    if matches!(host, Host::HerdrPlugin) {
        lines.push("Use the Herdr 'Open Codex for brgr' action to run or review work.".to_owned());
    }
    lines.push(String::new());
    for note in &model.notes {
        lines.push(human::safe_text(note));
    }
    let headers = [
        "TASK", "PROJECT", "HARNESS", "ALIAS", "MODEL", "REV", "STATE", "RESULT", "DECISION",
    ];
    let mut cells: Vec<[String; 9]> = model
        .rows
        .iter()
        .map(|row| {
            [
                human::cell(&row.task_id.to_string(), TASK_ID_CHARS),
                human::cell(&row.project, PROJECT_CAP),
                human::cell(&row.harness, HARNESS_CAP),
                row.alias
                    .as_deref()
                    .map_or_else(|| "-".to_owned(), |value| human::cell(value, ALIAS_CAP)),
                row.model
                    .as_deref()
                    .map_or_else(|| "-".to_owned(), |value| human::cell(value, MODEL_CAP)),
                row.revision.to_string(),
                human::cell(&row.state, 16),
                human::cell(&row.outcome, 11),
                human::cell(&row.verdict, 10),
            ]
        })
        .collect();
    let mut widths = [0_usize; 9];
    for (index, header) in headers.iter().enumerate() {
        widths[index] = measure_text_width(header);
    }
    let caps = [
        TASK_ID_CHARS,
        PROJECT_CAP,
        HARNESS_CAP,
        ALIAS_CAP,
        MODEL_CAP,
        4,
        16,
        11,
        10,
    ];
    for row in &cells {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(measure_text_width(cell)).min(caps[index]);
        }
    }
    if let Some((width, _)) = size {
        shrink_variable_columns(&mut widths, usize::from(width));
    }
    for row in &mut cells {
        for (index, cell) in row.iter_mut().enumerate() {
            *cell = clip_display(cell, widths[index]);
        }
    }
    lines.push(format_row(&headers.map(str::to_owned), &widths));
    if model.rows.is_empty() {
        let empty = if model.scope_line.starts_with("my tasks") {
            "No tasks for this session · pass --all to list every owner"
        } else {
            "No managed tasks yet."
        };
        lines.push(empty.to_owned());
    }
    for row in &cells {
        lines.push(format_row(row, &widths));
    }
    lines.push(String::new());
    let exit = match host {
        Host::Terminal => "Ctrl-C to exit",
        Host::HerdrPlugin => "close the pane to exit",
    };
    lines.push(format!(
        "Read-only · latest {BOARD_LIMIT} tasks · refreshes every 2 seconds · {exit}"
    ));
    if let Some((width, height)) = size {
        let width = usize::from(width);
        let height = usize::from(height).max(1);
        lines.truncate(height);
        for line in &mut lines {
            *line = clip_display(line, width);
        }
    }
    lines
}

/// The PROJECT cell: the source repository brgr recorded for a task worktree,
/// then the workspace folder when nothing better is recorded.
fn project_name(workspace: &str, checkout: Option<&str>) -> String {
    let source = checkout
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(workspace);
    Path::new(source)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("-")
        .to_owned()
}

pub(crate) fn alias_for<'a>(
    aliases: &'a BTreeMap<String, Alias>,
    harness: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Option<&'a str> {
    let model = model?;
    let mut exact = None;
    let mut without_effort = None;
    for (name, alias) in aliases {
        if alias.harness != harness {
            continue;
        }
        let Some(alias_model) = alias.model.as_deref() else {
            continue;
        };
        if alias_model != model {
            continue;
        }
        match alias.effort.as_deref() {
            Some(value) if Some(value) == effort => {
                if exact.is_none() {
                    exact = Some(name.as_str());
                }
            }
            None => {
                if without_effort.is_none() {
                    without_effort = Some(name.as_str());
                }
            }
            Some(_) => {}
        }
    }
    exact.or(without_effort)
}

fn shrink_variable_columns(widths: &mut [usize; 9], terminal_width: usize) {
    let needed = |widths: &[usize; 9]| widths.iter().sum::<usize>() + 8;
    for index in [4_usize, 1, 2] {
        while needed(widths) > terminal_width && widths[index] > SHRINK_FLOOR {
            widths[index] -= 1;
        }
    }
}

fn format_row(cells: &[String], widths: &[usize; 9]) -> String {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(&pad_display(cell, widths[index]));
    }
    line
}

fn pad_display(cell: &str, width: usize) -> String {
    let current = measure_text_width(cell);
    if current >= width {
        cell.to_owned()
    } else {
        format!("{cell}{}", " ".repeat(width - current))
    }
}

fn clip_display(text: &str, width: usize) -> String {
    if measure_text_width(text) <= width {
        return text.to_owned();
    }
    let mut clipped = String::new();
    for character in text.chars() {
        let mut candidate = clipped.clone();
        candidate.push(character);
        if measure_text_width(&candidate) > width {
            break;
        }
        clipped = candidate;
    }
    clipped
}

pub(crate) fn refresh_failed_lines(error: &str) -> Vec<String> {
    vec![
        "brgr · managed tasks".to_owned(),
        String::new(),
        format!("board refresh failed: {}", human::safe_text(error)),
        "No tasks were mutated or reconciled.".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use brgr_protocol::TaskId;
    use std::fs;

    fn alias(harness: &str, model: Option<&str>, effort: Option<&str>) -> Alias {
        Alias {
            harness: harness.to_owned(),
            model: model.map(ToOwned::to_owned),
            effort: effort.map(ToOwned::to_owned),
        }
    }

    fn row(project: &str) -> BoardRow {
        BoardRow {
            task_id: TaskId::new(),
            project: project.to_owned(),
            harness: "local.fixture".to_owned(),
            alias: Some("grok".to_owned()),
            model: Some("fixture-model".to_owned()),
            revision: 1,
            state: "terminal".to_owned(),
            outcome: "candidate".to_owned(),
            verdict: "-".to_owned(),
        }
    }

    #[test]
    fn project_uses_the_recorded_repository_not_the_worktree_folder() {
        assert_eq!(
            project_name(
                "/home/brgr/worktrees/brgr-live3-repo/5b8deeaa",
                Some("/src/brgr-live3-repo"),
            ),
            "brgr-live3-repo"
        );
    }

    #[test]
    fn project_falls_back_to_the_workspace_folder_without_a_checkout() {
        assert_eq!(project_name("/tmp/work", None), "work");
        assert_eq!(project_name("/tmp/work", Some("  ")), "work");
    }

    #[test]
    fn alias_matches_harness_model_and_effort() {
        let mut aliases = BTreeMap::new();
        aliases.insert(
            "bare".to_owned(),
            alias("local.opencode", Some("kimi"), None),
        );
        aliases.insert(
            "exact".to_owned(),
            alias("local.opencode", Some("kimi"), Some("high")),
        );
        aliases.insert(
            "other".to_owned(),
            alias("local.opencode", Some("kimi"), Some("low")),
        );
        assert_eq!(
            alias_for(&aliases, "local.opencode", Some("kimi"), Some("high")),
            Some("exact")
        );
        assert_eq!(
            alias_for(&aliases, "local.opencode", Some("kimi"), Some("low")),
            Some("other")
        );
        assert_eq!(
            alias_for(&aliases, "local.opencode", Some("kimi"), None),
            Some("bare")
        );
    }

    #[test]
    fn alias_without_model_never_labels_a_row() {
        let mut aliases = BTreeMap::new();
        aliases.insert("bare".to_owned(), alias("local.fixture", None, None));
        assert_eq!(
            alias_for(&aliases, "local.fixture", Some("fixture-model"), None),
            None
        );
    }

    #[test]
    fn two_aliases_for_one_route_pick_the_first_name() {
        let mut aliases = BTreeMap::new();
        aliases.insert("zeta".to_owned(), alias("local.fixture", Some("m"), None));
        aliases.insert("alpha".to_owned(), alias("local.fixture", Some("m"), None));
        assert_eq!(
            alias_for(&aliases, "local.fixture", Some("m"), None),
            Some("alpha")
        );
    }

    #[test]
    fn board_header_names_the_owner_session_not_codex() {
        let model = BoardModel {
            scope_line: "my tasks · session 1a2b3c4d…".to_owned(),
            rows: vec![row("work")],
            notes: Vec::new(),
        };
        let text = render(&model, Host::Terminal, None).join("\n");
        assert!(text.contains("my tasks · session 1a2b3c4d…"), "{text}");
        assert!(text.contains("The owner session decides"), "{text}");
        assert!(text.contains("This board never decides"), "{text}");
        assert!(!text.contains("Codex owns"), "{text}");
        assert!(text.contains("TASK"), "{text}");
        assert!(text.contains("ALIAS"), "{text}");
        assert!(text.contains("MODEL"), "{text}");
    }

    #[test]
    fn board_lines_carry_no_control_or_bidi_characters() {
        let model = BoardModel {
            scope_line: "my tasks · session sess\u{1b}[31m\u{202e}".to_owned(),
            rows: vec![BoardRow {
                task_id: TaskId::new(),
                project: "repo\n\u{1b}[31m\u{202e}\u{200b}".to_owned(),
                harness: "h\u{1b}arn".to_owned(),
                alias: Some("a\u{202e}l".to_owned()),
                model: Some("m\u{200b}odel".to_owned()),
                revision: 1,
                state: "running".to_owned(),
                outcome: "-".to_owned(),
                verdict: "-".to_owned(),
            }],
            notes: vec!["failed \u{1b}[2J".to_owned()],
        };
        let text = render(&model, Host::Terminal, None).join("\n");
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{202e}'), "{text:?}");
        assert!(!text.contains('\u{200b}'), "{text:?}");
        assert!(!text.contains('\n') || text.lines().count() > 1);
        for line in text.lines() {
            assert!(!line.contains('\n'));
        }
    }

    #[test]
    fn wide_project_name_keeps_columns_aligned() {
        let model = BoardModel {
            scope_line: "all owners".to_owned(),
            rows: vec![row("프로젝트")],
            notes: Vec::new(),
        };
        let lines = render(&model, Host::Terminal, None);
        let header = lines
            .iter()
            .find(|line| line.starts_with("TASK"))
            .expect("header");
        let row = lines
            .iter()
            .find(|line| line.contains("프로젝트"))
            .expect("row");
        let harness_at = header.find("HARNESS").expect("HARNESS");
        assert_eq!(
            measure_text_width(&header[..harness_at]),
            measure_text_width(&row[..row.find("local.fixture").expect("harness")])
        );
    }

    #[test]
    fn narrow_terminal_clips_lines_to_width() {
        let model = BoardModel {
            scope_line: "all owners".to_owned(),
            rows: vec![row("very-long-project-folder")],
            notes: Vec::new(),
        };
        let lines = render(&model, Host::Terminal, Some((24, 8)));
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(
                measure_text_width(line) <= 24,
                "line wider than 24: {line:?} ({})",
                measure_text_width(line)
            );
        }
        assert!(lines.len() <= 8);
    }

    #[test]
    fn unloadable_config_still_renders_the_board() {
        let root = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(Some(root.path().join("brgr"))).unwrap();
        fs::write(&paths.config, "this is not toml [[").unwrap();
        let model = load(&paths, &Scope::All).unwrap();
        assert!(
            model
                .notes
                .iter()
                .any(|note| note.contains("aliases not shown: config.toml did not load")),
            "{:?}",
            model.notes
        );
        let text = render(&model, Host::Terminal, None).join("\n");
        assert!(text.contains("brgr · managed tasks"), "{text}");
        assert!(text.contains("No managed tasks yet."), "{text}");
        assert!(text.contains("aliases not shown"), "{text}");
    }
}
