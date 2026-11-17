//! Extension points the optional orchestrator installs into the core.
//!
//! The durable core (admission, pane trust, notice delivery) runs without the
//! orchestrator. The orchestrator plugs its own behaviour in here instead, so
//! no core module names `workflow`, `head`, `debate`, `adversary` or
//! `handoff_condition`.
//!
//! The defaults keep a run without the orchestrator meaningful: an admission
//! divergence still reports the journaled step, no recorded pane is trusted,
//! and a parallel limit is still a retryable refusal.

use std::sync::OnceLock;

use anyhow::Error;
use brgr_store::{WorkflowRunId, WorkflowStepKey};

/// What an orchestrator installs at startup.
#[derive(Clone, Copy)]
pub(crate) struct Hooks {
    /// The error an admission reports when a workflow step's input diverged.
    pub(crate) diverged_admission: fn(&WorkflowStepKey) -> Error,
    /// Whether a recorded pane is trusted for this process's workflow run.
    pub(crate) trusts_recorded_pane: fn(&str) -> bool,
    /// Ends a process whose run is at its parallel limit: a retryable refusal.
    pub(crate) parallel_limit_exit: fn(&WorkflowRunId) -> !,
}

fn default_diverged_admission(step_key: &WorkflowStepKey) -> Error {
    anyhow::anyhow!(
        "workflow step {step_key} diverged: the input does not match the journaled step"
    )
}

fn default_trusts_recorded_pane(_pane: &str) -> bool {
    false
}

fn default_parallel_limit_exit(run_id: &WorkflowRunId) -> ! {
    eprintln!("workflow run {run_id} is at its parallel limit; retry later");
    std::process::exit(75);
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// Installs the orchestrator's hooks. The first install wins; a later one is
/// ignored so a process never switches behaviour part way through.
pub(crate) fn install(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}

fn hooks() -> &'static Hooks {
    HOOKS.get_or_init(|| Hooks {
        diverged_admission: default_diverged_admission,
        trusts_recorded_pane: default_trusts_recorded_pane,
        parallel_limit_exit: default_parallel_limit_exit,
    })
}

pub(crate) fn diverged_admission(step_key: &WorkflowStepKey) -> Error {
    (hooks().diverged_admission)(step_key)
}

pub(crate) fn trusts_recorded_pane(pane: &str) -> bool {
    (hooks().trusts_recorded_pane)(pane)
}

pub(crate) fn exit_parallel_limit(run_id: &WorkflowRunId) -> ! {
    (hooks().parallel_limit_exit)(run_id)
}
