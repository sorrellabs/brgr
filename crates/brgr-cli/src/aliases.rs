//! Named harness and model routes shared by CLI and future user interfaces.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use brgr_registry::CLAUDE_CODE_HARNESS_ID;

use crate::config::{ClaudeRoute, Config};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Alias {
    pub(crate) harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!(
            "alias name must be non-empty, must not start with '-', or contain whitespace/control characters"
        );
    }
    Ok(())
}

pub(crate) fn set(config: &mut Config, name: &str, alias: Alias) -> Result<()> {
    validate_name(name)?;
    if alias.harness.is_empty() {
        bail!("alias harness must not be empty");
    }
    for (label, value) in [("model", &alias.model), ("effort", &alias.effort)] {
        if value
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.chars().any(char::is_control))
        {
            bail!("alias {label} must be non-empty and contain no control characters");
        }
    }
    let model = alias
        .model
        .clone()
        .or_else(|| config.calling(&alias.harness).model);
    if config.routes.claude == ClaudeRoute::ClaudeCodeOnly
        && alias.harness != CLAUDE_CODE_HARNESS_ID
        && model
            .as_deref()
            .is_some_and(crate::claude_route::is_claude_model)
    {
        let model = model.expect("Claude model present after the recognition check");
        bail!(
            "claude-route is claude-code-only: alias {name} would run Claude model `{model}` on {}, which brgr would refuse. Point it at local.claude-code, or first run `brgr config set claude-route any`.",
            alias.harness
        );
    }
    config.aliases.insert(name.to_owned(), alias);
    Ok(())
}

pub(crate) fn remove(config: &mut Config, name: &str) -> Result<Alias> {
    validate_name(name)?;
    config
        .aliases
        .remove(name)
        .ok_or_else(|| anyhow::anyhow!("no alias named {name}"))
}

pub(crate) fn get<'a>(config: &'a Config, name: &str) -> Option<&'a Alias> {
    config.aliases.get(name)
}

pub(crate) fn list(config: &Config) -> &BTreeMap<String, Alias> {
    &config.aliases
}

pub(crate) fn known(aliases: &BTreeMap<String, Alias>) -> String {
    if aliases.is_empty() {
        "(none)".to_owned()
    } else {
        aliases.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fable() -> Alias {
        Alias {
            harness: "local.opencode".into(),
            model: Some("litellm/claude-fable-5-2".into()),
            effort: None,
        }
    }

    #[test]
    fn an_alias_that_would_run_claude_on_another_harness_is_refused_under_claude_code_only() {
        let mut config = Config::default();
        let error = set(&mut config, "fable", fable()).unwrap_err().to_string();
        assert_eq!(
            error,
            "claude-route is claude-code-only: alias fable would run Claude model `litellm/claude-fable-5-2` on local.opencode, which brgr would refuse. Point it at local.claude-code, or first run `brgr config set claude-route any`."
        );
        assert!(config.aliases.is_empty());
    }

    #[test]
    fn the_same_alias_is_saved_once_claude_route_is_any() {
        let mut config = Config::default();
        config.set("claude-route", "any", None).unwrap();
        set(&mut config, "fable", fable()).unwrap();
        assert_eq!(
            config.aliases["fable"].model.as_deref(),
            Some("litellm/claude-fable-5-2")
        );
    }
}
