//! The display name of a worker pane.

/// The values a pane name template can use.
pub(crate) struct Input<'a> {
    pub(crate) harness: &'a str,
    pub(crate) model: Option<&'a str>,
    pub(crate) effort: Option<&'a str>,
    pub(crate) objective: &'a str,
    pub(crate) task: &'a str,
}

/// How much of the objective one line may show.
const OBJECTIVE_LIMIT: usize = 48;
/// How long a whole pane name may be.
const NAME_LIMIT: usize = 120;
/// Herdr 0.9.5 agent names: `[a-z][a-z0-9_-]{0,31}`.
const AGENT_NAME_LIMIT: usize = 32;
/// Task-id characters kept so two live agents do not share a name.
const TASK_PREFIX: usize = 8;

/// A Herdr agent name from the run: `{harness-short}-{task8}`, lowercase,
/// `[a-z][a-z0-9_-]{0,31}`. Pure: no process, no Herdr. The pane label is
/// separate and may keep punctuation Herdr would reject here.
pub(crate) fn agent_name(harness: &str, task: &str) -> String {
    let harness = harness.strip_prefix("local.").unwrap_or(harness);
    let sanitized: String = harness
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || *character == '_'
                || *character == '-'
        })
        .collect();
    let mut short = sanitized
        .split('-')
        .find(|piece| !piece.is_empty())
        .unwrap_or("h")
        .to_owned();
    if !short.starts_with(|character: char| character.is_ascii_lowercase()) {
        short.insert(0, 'h');
    }
    let task: String = task
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        .take(TASK_PREFIX)
        .collect();
    let suffix = if task.is_empty() {
        String::new()
    } else {
        format!("-{task}")
    };
    let budget = AGENT_NAME_LIMIT.saturating_sub(suffix.len()).max(1);
    let short: String = short.chars().take(budget).collect();
    let mut name = format!("{short}{suffix}");
    name.truncate(AGENT_NAME_LIMIT);
    if name.is_empty() || !name.starts_with(|character: char| character.is_ascii_lowercase()) {
        name.insert(0, 'h');
        name.truncate(AGENT_NAME_LIMIT);
    }
    name
}

/// Renders a pane name from `template`. Pure: no process, no Herdr.
pub(crate) fn render(template: &str, input: &Input<'_>) -> String {
    if template.is_empty() {
        return String::new();
    }
    let objective = sanitize_objective(input.objective);
    let mut piece = cut_objective(&objective);
    let mut name = render_template(template, input, &piece);
    if name.chars().count() > NAME_LIMIT {
        // The objective is the only part that yields: keep the route readable
        // and cut the task's own words further.
        let overhead = name.chars().count().saturating_sub(piece.chars().count());
        let available = NAME_LIMIT.saturating_sub(overhead);
        piece = fit_objective(&objective, available);
        name = render_template(template, input, &piece);
    }
    name.retain(|character| !matches!(character, '\n' | '\r'));
    name.chars().take(NAME_LIMIT).collect()
}

#[derive(Debug)]
enum Token<'a> {
    Literal(&'a str),
    Field(&'a str),
}

fn tokens(template: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open + 1..].find('}').map(|at| open + 1 + at) else {
            break;
        };
        if open > 0 {
            tokens.push(Token::Literal(&rest[..open]));
        }
        tokens.push(Token::Field(&rest[open + 1..close]));
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        tokens.push(Token::Literal(rest));
    }
    tokens
}

fn render_template(template: &str, input: &Input<'_>, objective: &str) -> String {
    let (provider, model) = split_model(input.harness, input.model);
    let harness = input
        .harness
        .strip_prefix("local.")
        .unwrap_or(input.harness);
    let task: String = input.task.chars().take(8).collect();
    let tokens = tokens(template);
    let mut out = String::new();
    let mut skip_slash = false;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Literal(text) => {
                let text = if skip_slash {
                    skip_slash = false;
                    text.strip_prefix('/').unwrap_or(text)
                } else {
                    text
                };
                out.push_str(text);
            }
            Token::Field(field) => {
                let value = match *field {
                    "harness" => Some(harness),
                    "provider" => provider.as_deref(),
                    "model" => model.as_deref(),
                    "effort" => input.effort,
                    "objective" => Some(objective),
                    "task" => Some(task.as_str()),
                    _ => None,
                };
                match value {
                    Some(value) if !value.is_empty() => out.push_str(value),
                    // A missing field takes one adjacent `/` with it, so the
                    // name never shows a bare `//` or a trailing slash.
                    _ => {
                        if out.ends_with('/') {
                            out.pop();
                        } else if matches!(
                            tokens.get(index + 1),
                            Some(Token::Literal(next)) if next.starts_with('/')
                        ) {
                            skip_slash = true;
                        }
                    }
                }
            }
        }
    }
    out.trim().to_owned()
}

/// The provider the model itself names, or the harness's fixed one, and the
/// model without that prefix.
fn split_model(harness: &str, model: Option<&str>) -> (Option<String>, Option<String>) {
    let requested = model.map(str::trim).filter(|model| !model.is_empty());
    match requested {
        Some(model) => match model.split_once('/') {
            Some((provider, rest)) => {
                let provider = provider.trim();
                let rest = rest.trim();
                (
                    (!provider.is_empty()).then(|| provider.to_owned()),
                    (!rest.is_empty()).then(|| rest.to_owned()),
                )
            }
            None => (fixed_provider(harness), Some(model.to_owned())),
        },
        None => (fixed_provider(harness), None),
    }
}

fn fixed_provider(harness: &str) -> Option<String> {
    let harness = harness.strip_prefix("local.").unwrap_or(harness);
    let provider = match harness {
        "claude-code" => "anthropic",
        "codex" => "openai",
        "gemini" => "google",
        "copilot" => "github",
        "cursor-cli" => "cursor",
        _ => return None,
    };
    Some(provider.to_owned())
}

fn sanitize_objective(objective: &str) -> String {
    let first = objective.split('\n').next().unwrap_or_default();
    let cleaned: String = first
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first `OBJECTIVE_LIMIT` characters, with `…` when there was more.
fn cut_objective(objective: &str) -> String {
    if objective.chars().count() <= OBJECTIVE_LIMIT {
        return objective.to_owned();
    }
    let mut out: String = objective.chars().take(OBJECTIVE_LIMIT).collect();
    out.push('…');
    out
}

/// At most `available` characters, with `…` when the objective had to be cut
/// again to fit the whole name.
fn fit_objective(objective: &str, available: usize) -> String {
    if objective.chars().count() <= available {
        return objective.to_owned();
    }
    if available == 0 {
        return String::new();
    }
    let mut out: String = objective.chars().take(available - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = "fe3142c6-d800-485e-a612-d69de695b2b8";
    const DEFAULT: &str = "{harness}/{provider}/{model}: {objective}";

    fn input<'a>(
        harness: &'a str,
        model: Option<&'a str>,
        effort: Option<&'a str>,
        objective: &'a str,
    ) -> Input<'a> {
        Input {
            harness,
            model,
            effort,
            objective,
            task: TASK,
        }
    }

    #[test]
    fn a_model_with_a_provider_prefix_supplies_the_provider_and_the_rest() {
        assert_eq!(
            render(
                DEFAULT,
                &input(
                    "local.cursor-cli",
                    Some("xai/grok-4.7-medium"),
                    None,
                    "brgr watch 구현"
                )
            ),
            "cursor-cli/xai/grok-4.7-medium: brgr watch 구현"
        );
        assert_eq!(
            render(
                DEFAULT,
                &input(
                    "local.command-code",
                    Some("deepseek/deepseek-v4.1-flash"),
                    None,
                    "fix stash note"
                )
            ),
            "command-code/deepseek/deepseek-v4.1-flash: fix stash note"
        );
    }

    #[test]
    fn a_model_without_a_prefix_uses_the_fixed_harness_table() {
        let cases = [
            ("local.claude-code", "anthropic"),
            ("local.codex", "openai"),
            ("local.gemini", "google"),
            ("local.copilot", "github"),
            ("local.cursor-cli", "cursor"),
        ];
        for (harness, provider) in cases {
            let short = harness.trim_start_matches("local.");
            assert_eq!(
                render(DEFAULT, &input(harness, Some("model-x"), None, "x")),
                format!("{short}/{provider}/model-x: x"),
                "{harness}"
            );
        }
    }

    #[test]
    fn a_missing_model_drops_its_separator_and_keeps_the_provider() {
        assert_eq!(
            render(DEFAULT, &input("local.claude-code", None, None, "x")),
            "claude-code/anthropic: x"
        );
        for (harness, provider) in [
            ("local.claude-code", "anthropic"),
            ("local.codex", "openai"),
            ("local.gemini", "google"),
            ("local.copilot", "github"),
            ("local.cursor-cli", "cursor"),
        ] {
            let short = harness.trim_start_matches("local.");
            assert_eq!(
                render(DEFAULT, &input(harness, None, None, "x")),
                format!("{short}/{provider}: x"),
                "{harness}"
            );
        }
    }

    #[test]
    fn a_missing_provider_and_model_never_leaves_a_double_separator() {
        assert_eq!(
            render(DEFAULT, &input("local.acme", None, None, "x")),
            "acme: x"
        );
        assert_eq!(
            render(DEFAULT, &input("local.acme", Some("gpt-5"), None, "x")),
            "acme/gpt-5: x"
        );
        assert_eq!(
            render(
                "{harness}/{provider}/{model}: {objective}",
                &input("local.cursor-cli", Some("xai/grok"), None, "x")
            ),
            "cursor-cli/xai/grok: x"
        );
    }

    #[test]
    fn the_objective_is_its_first_line_with_control_characters_removed() {
        assert_eq!(
            render(
                "{objective}",
                &input("local.claude-code", None, None, "first line\nsecond line")
            ),
            "first line"
        );
        assert_eq!(
            render(
                "{objective}",
                &input("local.claude-code", None, None, "a\tb\r c\u{7}  d")
            ),
            "ab c d"
        );
    }

    #[test]
    fn a_multibyte_objective_is_cut_without_splitting_a_character() {
        let objective: String = "가".repeat(60);
        let name = render(
            "{objective}",
            &input("local.claude-code", None, None, &objective),
        );
        let chars: Vec<char> = name.chars().collect();
        assert_eq!(chars.len(), 49, "{name}");
        assert_eq!(chars[..48], ['가'; 48], "{name}");
        assert_eq!(chars[48], '…', "{name}");
        assert!(!name.contains('\u{fffd}'), "{name}");
    }

    #[test]
    fn the_whole_name_is_at_most_120_characters() {
        let objective: String = "목표 ".repeat(80);
        let model: String = "m".repeat(100);
        let name = render(
            DEFAULT,
            &input("local.cursor-cli", Some(&model), None, &objective),
        );
        assert!(name.chars().count() <= 120, "{name}");
        assert!(!name.contains('\n'));
        assert!(name.starts_with("cursor-cli/cursor/"), "{name}");
    }

    #[test]
    fn effort_and_task_placeholders_render() {
        assert_eq!(
            render(
                "{harness}/{effort}/{task}: {objective}",
                &input("local.claude-code", None, Some("high"), "x")
            ),
            "claude-code/high/fe3142c6: x"
        );
    }

    #[test]
    fn an_empty_template_renders_nothing() {
        assert_eq!(render("", &input("local.claude-code", None, None, "x")), "");
    }

    fn herdr_agent_name(name: &str) -> bool {
        let bytes = name.as_bytes();
        (1..=AGENT_NAME_LIMIT).contains(&bytes.len())
            && bytes[0].is_ascii_lowercase()
            && bytes.iter().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
    }

    #[test]
    fn an_agent_name_is_the_harness_short_and_task_prefix() {
        assert_eq!(
            agent_name("local.claude-code", "1cf44e8d-c0aa-43d7-9603-0079d032a5d8"),
            "claude-1cf44e8d"
        );
    }

    #[test]
    fn an_agent_name_uses_only_the_herdr_charset() {
        let name = agent_name("local.claude-code", TASK);
        assert!(
            name.chars().all(|character| character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '_'
                || character == '-'),
            "{name}"
        );
        assert!(herdr_agent_name(&name), "{name}");
    }

    #[test]
    fn an_agent_name_is_at_most_32_characters() {
        let harness = "x".repeat(80);
        let name = agent_name(&harness, TASK);
        assert!(name.len() <= AGENT_NAME_LIMIT, "{name}");
        assert!(herdr_agent_name(&name), "{name}");
        assert!(
            name.contains(&TASK.chars().take(TASK_PREFIX).collect::<String>()),
            "{name}"
        );
    }

    #[test]
    fn an_agent_name_starts_with_a_letter() {
        for harness in ["9lives", "123Harness", "---", "!!!", "_foo", ""] {
            let name = agent_name(harness, TASK);
            assert!(
                name.starts_with(|character: char| character.is_ascii_lowercase()),
                "{harness:?} → {name}"
            );
            assert!(herdr_agent_name(&name), "{harness:?} → {name}");
        }
    }

    #[test]
    fn agent_names_differ_when_the_task_id_differs() {
        let a = agent_name("local.claude-code", TASK);
        let b = agent_name("local.claude-code", "9bb22c11-bbbb-485e-a612-d69de695b2b8");
        assert_ne!(a, b, "{a} {b}");
        assert!(a.ends_with("fe3142c6"), "{a}");
        assert!(b.ends_with("9bb22c11"), "{b}");
    }

    #[test]
    fn an_agent_name_matches_the_herdr_pattern_for_odd_harness_ids_and_labels() {
        let labels = [
            "claude-code/anthropic: Explain the build",
            "!!!",
            "123",
            "",
            "Name With Spaces",
        ];
        let harnesses = [
            "local.claude-code",
            "123Harness",
            "CLAUDE CODE",
            "!!!",
            "---",
            "local.cursor-cli",
            "9lives",
            "Foo.Bar/Baz",
            "command-code",
        ];
        for harness in harnesses {
            for label in labels {
                let name = agent_name(harness, TASK);
                assert!(herdr_agent_name(&name), "{harness} / {label:?} → {name}");
                assert!(
                    !name.contains('/') && !name.contains(':') && !name.contains(' '),
                    "the pane label leaked into the agent name: {name}"
                );
            }
        }
        let long = "H".repeat(80);
        assert!(herdr_agent_name(&agent_name(&long, TASK)), "{long}");
    }
}
