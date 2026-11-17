//! The durable core must not depend on the optional orchestrator.
//!
//! Issue #431 splits brgr into a core, a surface and an optional orchestrator.
//! The core layer below is what stays when the orchestrator is not installed,
//! so none of it may name an orchestrator module. The core reaches the
//! orchestrator only through `extension`.

use std::{fs, path::Path};

/// Core-layer modules inside `brgr-cli`, from the #431 layer table.
const CORE_MODULES: &[&str] = &[
    "admission",
    "supervision",
    "evidence",
    "native_result",
    "message",
    "notification",
    "failure_banner",
    "error_memo",
    "config",
    "worker_identity",
    "session_model",
    "claude_route",
    "extension",
];

/// The orchestrator modules the core must not import.
const ORCHESTRATOR_MODULES: &[&str] = &[
    "workflow",
    "head",
    "debate",
    "adversary",
    "handoff_condition",
];

#[test]
fn core_layer_does_not_import_the_orchestrator() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for module in CORE_MODULES {
        let path = src.join(format!("{module}.rs"));
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
        for (index, line) in source.lines().enumerate() {
            for orchestrator in ORCHESTRATOR_MODULES {
                let import = format!("crate::{orchestrator}");
                if line.contains(&import) {
                    violations.push(format!(
                        "{}:{} names {import}: {}",
                        path.display(),
                        index + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "core-layer modules must not import the orchestrator:\n{}",
        violations.join("\n")
    );
}
