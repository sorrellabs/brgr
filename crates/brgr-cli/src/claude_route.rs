//! Whether a Claude model may run only through Claude Code, or on any harness.

use brgr_registry::CLAUDE_CODE_HARNESS_ID;

use crate::config::{ClaudeRoute, Config};

const CLAUDE_SHORTHANDS: &[&str] = &["opus", "sonnet", "haiku", "fable"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    Run { alias: Option<String> },
    Revise,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct ClaudeRouteRefused(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Violation {
    pub(crate) kind: ViolationKind,
    pub(crate) harness: String,
    pub(crate) model: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ViolationKind {
    Alias(String),
    DefaultModel,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ViolationKind::Alias(name) => {
                write!(f, "alias {name} ({}, {})", self.harness, self.model)
            }
            ViolationKind::DefaultModel => {
                write!(f, "default model of {} ({})", self.harness, self.model)
            }
        }
    }
}

pub(crate) fn is_claude_model(model: &str) -> bool {
    let lowered = model.to_ascii_lowercase();
    let mut parts = lowered.split('/');
    let Some(name) = parts.next_back() else {
        return false;
    };
    if parts.any(|provider| provider == "anthropic") {
        return true;
    }
    if name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(is_claude_token)
    {
        return true;
    }
    let bare = name.split([':', '#', '@']).next().unwrap_or(name);
    CLAUDE_SHORTHANDS.contains(&bare)
}

fn is_claude_token(token: &str) -> bool {
    token == "anthropic"
        || token == "claude"
        || token
            .strip_prefix("claude")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

pub(crate) fn check(
    route: ClaudeRoute,
    harness: &str,
    model: Option<&str>,
    origin: &Origin,
) -> Result<(), ClaudeRouteRefused> {
    if route == ClaudeRoute::Any
        || harness == CLAUDE_CODE_HARNESS_ID
        || !model.is_some_and(is_claude_model)
    {
        return Ok(());
    }
    let model = model.expect("Claude model present after the recognition check");
    let alias = match origin {
        Origin::Run { alias: Some(name) } => format!(" (alias {name})"),
        Origin::Run { alias: None } | Origin::Revise => String::new(),
    };
    let second = match origin {
        Origin::Run { .. } => {
            "Use `--harness local.claude-code` with a model it accepts, or allow other harnesses with `brgr config set claude-route any`."
        }
        Origin::Revise => {
            "A revision keeps its route: start a new task on `local.claude-code`, or allow other harnesses with `brgr config set claude-route any`."
        }
    };
    Err(ClaudeRouteRefused(format!(
        "claude-route is claude-code-only: Claude model `{model}`{alias} cannot run on {harness}. {second} Nothing was started and no other harness was chosen."
    )))
}

pub(crate) fn violations(config: &Config) -> Vec<Violation> {
    if config.routes.claude != ClaudeRoute::ClaudeCodeOnly {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (name, alias) in &config.aliases {
        let Some(model) = alias
            .model
            .clone()
            .or_else(|| config.calling(&alias.harness).model)
        else {
            continue;
        };
        if alias.harness != CLAUDE_CODE_HARNESS_ID && is_claude_model(&model) {
            found.push(Violation {
                kind: ViolationKind::Alias(name.clone()),
                harness: alias.harness.clone(),
                model,
            });
        }
    }
    let mut harnesses: std::collections::BTreeSet<String> =
        config.harnesses.keys().cloned().collect();
    if let Some(default) = &config.defaults.harness {
        harnesses.insert(default.clone());
    }
    for id in harnesses {
        if id == CLAUDE_CODE_HARNESS_ID {
            continue;
        }
        let Some(model) = config.calling(&id).model else {
            continue;
        };
        if is_claude_model(&model) {
            found.push(Violation {
                kind: ViolationKind::DefaultModel,
                harness: id,
                model,
            });
        }
    }
    found
}

pub(crate) fn format_violations(found: &[Violation], separator: &str) -> String {
    found
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(separator)
}

pub(crate) fn saved_route_refusal(config: &Config) -> Option<String> {
    let found = violations(config);
    if found.is_empty() {
        return None;
    }
    Some(format!(
        "claude-route is claude-code-only, but these saved routes run a Claude model on another harness: {}. Repoint or remove them, or run `brgr config set claude-route any`.",
        format_violations(&found, "; ")
    ))
}

pub(crate) fn tightening_note(config: &Config) -> Option<String> {
    let found = violations(config);
    if found.is_empty() {
        return None;
    }
    Some(format!(
        "brgr: claude-route is now claude-code-only; these saved routes will be refused and were left unchanged: {}",
        format_violations(&found, ", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aliases::Alias;
    use crate::config::CallingOptions;

    #[test]
    fn claude_names_are_recognized_across_provider_prefixes_and_suffixes() {
        for name in [
            "Claude-Opus-5-5",
            "litellm/claude-opus-5-5#high",
            "vertex_ai/claude-opus-5@20260101",
            "anthropic/x",
            "openrouter/anthropic/x",
            "claude-fable-5-2",
            "litellm/claude-fable-5-2",
            "us.anthropic.claude-sonnet-5-5-v1:0",
            "claude-4.5-sonnet",
            "claude3",
        ] {
            assert!(is_claude_model(name), "{name}");
        }
    }

    #[test]
    fn other_vendors_and_near_misses_are_not_claude() {
        for name in [
            "gpt-6.1-sol",
            "litellm/grok-4.6",
            "litellm/gemini-4-argon",
            "opencode/big-pickle#high",
            "cursor-grok-4.6-high",
            "claudette-1",
            "octopus-2",
            "magnum-opus",
            "",
        ] {
            assert!(!is_claude_model(name), "{name}");
        }
    }

    #[test]
    fn a_bare_family_name_counts_only_as_the_whole_model_name() {
        for name in [
            "opus",
            "sonnet",
            "haiku",
            "fable",
            "sonnet:high",
            "opus#fast",
            "haiku@1",
            "fable:high",
        ] {
            assert!(is_claude_model(name), "{name}");
        }
        for name in [
            "magnum-opus",
            "octopus-2",
            "provider/sonnets",
            "sonnets",
            "my-fable",
        ] {
            assert!(!is_claude_model(name), "{name}");
        }
    }

    #[test]
    fn claude_code_only_refuses_a_claude_model_on_another_harness() {
        let claude = Some("litellm/claude-fable-5-2");
        let other = Some("litellm/grok-4.6");
        let origin = Origin::Run { alias: None };
        for (route, harness, model, refuse) in [
            (ClaudeRoute::Any, "local.opencode", claude, false),
            (
                ClaudeRoute::ClaudeCodeOnly,
                CLAUDE_CODE_HARNESS_ID,
                claude,
                false,
            ),
            (ClaudeRoute::ClaudeCodeOnly, "local.opencode", None, false),
            (ClaudeRoute::ClaudeCodeOnly, "local.opencode", other, false),
            (ClaudeRoute::ClaudeCodeOnly, "local.opencode", claude, true),
            (ClaudeRoute::ClaudeCodeOnly, "local.fixture", claude, true),
            (ClaudeRoute::ClaudeCodeOnly, "custom.claude", claude, true),
        ] {
            let result = check(route, harness, model, &origin);
            assert_eq!(result.is_err(), refuse, "{route:?} {harness} {model:?}");
        }
    }

    #[test]
    fn the_refusal_names_the_setting_the_model_the_harness_and_the_alias() {
        let error = check(
            ClaudeRoute::ClaudeCodeOnly,
            "local.opencode",
            Some("litellm/claude-fable-5-2"),
            &Origin::Run {
                alias: Some("fable".into()),
            },
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "claude-route is claude-code-only: Claude model `litellm/claude-fable-5-2` (alias fable) cannot run on local.opencode. Use `--harness local.claude-code` with a model it accepts, or allow other harnesses with `brgr config set claude-route any`. Nothing was started and no other harness was chosen."
        );
        let without_alias = check(
            ClaudeRoute::ClaudeCodeOnly,
            "local.opencode",
            Some("litellm/claude-fable-5-2"),
            &Origin::Run { alias: None },
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            without_alias,
            "claude-route is claude-code-only: Claude model `litellm/claude-fable-5-2` cannot run on local.opencode. Use `--harness local.claude-code` with a model it accepts, or allow other harnesses with `brgr config set claude-route any`. Nothing was started and no other harness was chosen."
        );
        let revise = check(
            ClaudeRoute::ClaudeCodeOnly,
            "local.opencode",
            Some("litellm/claude-fable-5-2"),
            &Origin::Revise,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            revise,
            "claude-route is claude-code-only: Claude model `litellm/claude-fable-5-2` cannot run on local.opencode. A revision keeps its route: start a new task on `local.claude-code`, or allow other harnesses with `brgr config set claude-route any`. Nothing was started and no other harness was chosen."
        );
    }

    #[test]
    fn saved_routes_list_aliases_and_harness_defaults() {
        let mut config = Config::default();
        config.aliases.insert(
            "fable".into(),
            Alias {
                harness: "local.opencode".into(),
                model: Some("litellm/claude-fable-5-2".into()),
                effort: None,
            },
        );
        config.harnesses.insert(
            "local.opencode".into(),
            CallingOptions {
                model: Some("litellm/claude-opus-5-5".into()),
                ..CallingOptions::default()
            },
        );
        assert_eq!(
            saved_route_refusal(&config).as_deref(),
            Some(
                "claude-route is claude-code-only, but these saved routes run a Claude model on another harness: alias fable (local.opencode, litellm/claude-fable-5-2); default model of local.opencode (litellm/claude-opus-5-5). Repoint or remove them, or run `brgr config set claude-route any`."
            )
        );
    }
}
