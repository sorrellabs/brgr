//! Collecting a run's final result from stdout, a file, or JSONL events.

use std::{io::Read as _, os::unix::fs::MetadataExt as _, path::Path, process::ExitStatus};

use crate::{
    HarnessManifest, ResultSource, RunRequest, RunnerError,
    argv::{Substitutions, substitute},
};

pub(crate) fn can_collect_result(
    status: Option<&ExitStatus>,
    manifest: &HarnessManifest,
    cancelled: bool,
    timed_out: bool,
    truncated: bool,
) -> bool {
    !cancelled
        && !timed_out
        && !truncated
        && status
            .and_then(ExitStatus::code)
            .is_some_and(|code| manifest.result.success_exit_codes.contains(&code))
}

pub(crate) fn collect_result(
    manifest: &HarnessManifest,
    workspace: &Path,
    values: &Substitutions<'_>,
    stdout: &[u8],
) -> Result<(Vec<u8>, Option<String>), RunnerError> {
    match &manifest.result.source {
        ResultSource::Stdout => Ok((stdout.to_vec(), None)),
        ResultSource::JsonlAssistantFinal => {
            extract_jsonl_assistant_final(stdout).map(|result| (result, None))
        }
        ResultSource::StreamJsonResult => extract_stream_json_result(stdout),
        ResultSource::File { path } => {
            let rendered = substitute(path, values)?;
            let relative = Path::new(&rendered);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(RunnerError::ResultPathOutsideWorkspace(rendered));
            }
            let result_path = workspace.join(relative);
            if !result_path
                .canonicalize()?
                .starts_with(workspace.canonicalize()?)
            {
                return Err(RunnerError::ResultPathOutsideWorkspace(rendered));
            }
            let metadata = std::fs::symlink_metadata(&result_path)?;
            if !metadata.file_type().is_file() {
                return Err(RunnerError::ResultNotRegularFile(result_path));
            }
            if metadata.len() > manifest.result.max_bytes {
                return Err(RunnerError::ResultTooLarge {
                    max_bytes: manifest.result.max_bytes,
                    observed_bytes: metadata.len(),
                });
            }
            let mut file = std::fs::File::open(&result_path)?;
            let opened = file.metadata()?;
            if !opened.is_file()
                || (metadata.dev(), metadata.ino(), metadata.len())
                    != (opened.dev(), opened.ino(), opened.len())
            {
                return Err(RunnerError::ResultNotRegularFile(result_path));
            }
            let mut bytes = Vec::new();
            file.by_ref()
                .take(manifest.result.max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > manifest.result.max_bytes {
                return Err(RunnerError::ResultTooLarge {
                    max_bytes: manifest.result.max_bytes,
                    observed_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                });
            }
            Ok((bytes, None))
        }
    }
}

/// The sealed result bytes, the observed model, and the harness session a
/// follow-up run may resume.
type Collected = (Vec<u8>, Option<String>, Option<String>);

pub(crate) fn collect_result_with_model(
    manifest: &HarnessManifest,
    request: &RunRequest<'_>,
    values: &Substitutions<'_>,
    stdout: &[u8],
) -> Result<Collected, RunnerError> {
    let (result, session_id) = collect_result(manifest, request.workspace, values, stdout)?;
    if u64::try_from(result.len()).unwrap_or(u64::MAX) > manifest.result.max_bytes {
        return Err(RunnerError::ResultTooLarge {
            max_bytes: manifest.result.max_bytes,
            observed_bytes: u64::try_from(result.len()).unwrap_or(u64::MAX),
        });
    }
    let observed_model =
        if !result.is_empty() && manifest.result.source == ResultSource::JsonlAssistantFinal {
            observe_jsonl_model(stdout, request.model)?
        } else if !result.is_empty() && manifest.result.source == ResultSource::StreamJsonResult {
            observe_stream_json_model(stdout, request.model)?
        } else {
            None
        };
    Ok((result, observed_model, session_id))
}

/// Qwen Code's `-o stream-json` terminal event, pinned to the 0.27.4 envelope:
/// `{"type":"result","subtype":"success","is_error":false,"result":..,"session_id":..}`
/// or an `error_during_execution` event with `error.message`. Other shapes in
/// that place — older envelopes named `response` or `messages`, a result
/// without text — are refused as typed errors, never guessed at.
///
/// Returns the report bytes and the session id a follow-up run may resume.
#[doc(hidden)]
pub fn extract_stream_json_result(stdout: &[u8]) -> Result<(Vec<u8>, Option<String>), RunnerError> {
    let mut result_event = None;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: serde_json::Value = serde_json::from_slice(line)?;
        if event.get("type").and_then(serde_json::Value::as_str) == Some("result") {
            result_event = Some(event);
        }
    }
    let event = result_event.ok_or(RunnerError::MissingResultEvent)?;
    let session_id = event
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    match event.get("is_error").and_then(serde_json::Value::as_bool) {
        Some(false) => {
            if event.get("subtype").and_then(serde_json::Value::as_str) != Some("success") {
                return Err(RunnerError::UnexpectedResultShape);
            }
            let text = event
                .get("result")
                .and_then(serde_json::Value::as_str)
                .ok_or(RunnerError::UnexpectedResultShape)?;
            if text.trim().is_empty() {
                return Err(RunnerError::MissingAssistantText);
            }
            Ok((text.as_bytes().to_vec(), session_id))
        }
        Some(true) => Err(RunnerError::AssistantError(
            event
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .filter(|message| !message.is_empty())
                .ok_or(RunnerError::UnexpectedResultShape)?
                .to_owned(),
        )),
        None => Err(RunnerError::UnexpectedResultShape),
    }
}

/// The model a Qwen Code run's own `assistant` events named. The same
/// consistency rule as the OMP stream: one model, matching a requested one,
/// or the observation is unavailable rather than guessed.
pub(crate) fn observe_stream_json_model(
    stdout: &[u8],
    requested: Option<&str>,
) -> Result<Option<String>, RunnerError> {
    let mut observed: Option<String> = None;
    let mut missing_identity = false;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: serde_json::Value = serde_json::from_slice(line)?;
        if event.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
            continue;
        }
        let identity = event
            .pointer("/message/model")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        match (&observed, identity) {
            (Some(previous), Some(current)) if previous != &current => {
                return Err(RunnerError::MixedObservedModels);
            }
            (None, Some(current)) => observed = Some(current),
            (_, None) => missing_identity = true,
            _ => {}
        }
    }
    if missing_identity {
        return if requested.is_some() || observed.is_some() {
            Err(RunnerError::ObservedModelUnavailable)
        } else {
            Ok(None)
        };
    }
    if let Some(requested) = requested {
        let actual = observed
            .as_deref()
            .ok_or(RunnerError::ObservedModelUnavailable)?;
        if actual != requested {
            return Err(RunnerError::ObservedModelMismatch {
                requested: requested.to_owned(),
                observed: actual.to_owned(),
            });
        }
    }
    Ok(observed)
}

pub(crate) fn extract_jsonl_assistant_final(stdout: &[u8]) -> Result<Vec<u8>, RunnerError> {
    let mut final_assistant = None;
    let mut completed = false;
    let mut agent_failed = false;
    let mut assistant_turn_ended = false;
    let mut unqualified_agent_end = false;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: serde_json::Value = serde_json::from_slice(line)?;
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("message_end")
                if event
                    .pointer("/message/role")
                    .and_then(serde_json::Value::as_str)
                    == Some("assistant") =>
            {
                let message = event.get("message").expect("role came from message");
                let text = message
                    .pointer("/content")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|part| {
                        part.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    })
                    .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                final_assistant = Some((
                    text,
                    message
                        .get("errorMessage")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                ));
            }
            Some("agent_end") => {
                match event.get("stopReason").and_then(serde_json::Value::as_str) {
                    Some("completed") => completed = true,
                    Some("error") => agent_failed = true,
                    None => unqualified_agent_end = true,
                    _ => {}
                }
            }
            Some("turn_end")
                if event
                    .pointer("/message/role")
                    .and_then(serde_json::Value::as_str)
                    == Some("assistant") =>
            {
                assistant_turn_ended = true;
            }
            _ => {}
        }
    }
    if !(completed || agent_failed || (unqualified_agent_end && assistant_turn_ended)) {
        return Err(RunnerError::MissingTerminalEvent);
    }
    let (text, error) = final_assistant.ok_or(RunnerError::MissingAssistantText)?;
    if agent_failed {
        if let Some(error) = error.filter(|value| !value.is_empty()) {
            return Err(RunnerError::AssistantError(error));
        }
        return Err(RunnerError::MissingAssistantText);
    }
    if !text.trim().is_empty() {
        return Ok(text.into_bytes());
    }
    if let Some(error) = error.filter(|value| !value.is_empty()) {
        return Err(RunnerError::AssistantError(error));
    }
    Err(RunnerError::MissingAssistantText)
}

pub(crate) fn observe_jsonl_model(
    stdout: &[u8],
    requested: Option<&str>,
) -> Result<Option<String>, RunnerError> {
    let mut observed: Option<String> = None;
    let mut missing_identity = false;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: serde_json::Value = serde_json::from_slice(line)?;
        if event.get("type").and_then(serde_json::Value::as_str) != Some("message_end")
            || event
                .pointer("/message/role")
                .and_then(serde_json::Value::as_str)
                != Some("assistant")
        {
            continue;
        }
        let identity = event
            .pointer("/message/provider")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .zip(
                event
                    .pointer("/message/model")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty()),
            )
            .map(|(provider, model)| format!("{provider}/{model}"));
        match (&observed, identity) {
            (Some(previous), Some(current)) if previous != &current => {
                return Err(RunnerError::MixedObservedModels);
            }
            (None, Some(current)) => observed = Some(current),
            (_, None) => missing_identity = true,
            _ => {}
        }
    }
    if missing_identity {
        return if requested.is_some() || observed.is_some() {
            Err(RunnerError::ObservedModelUnavailable)
        } else {
            Ok(None)
        };
    }
    if let Some(requested) = requested {
        let actual = observed
            .as_deref()
            .ok_or(RunnerError::ObservedModelUnavailable)?;
        if actual != requested {
            return Err(RunnerError::ObservedModelMismatch {
                requested: requested.to_owned(),
                observed: actual.to_owned(),
            });
        }
    }
    Ok(observed)
}
