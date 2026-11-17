//! Keeps a machine from idle-sleeping while brgr starts a task.
//!
//! A start runs several slow steps (the harness health probe, the model
//! catalog, the worktree, the pane). A machine that sleeps in the middle of
//! them wakes with the network not yet up, and the step fails or waits out its
//! deadline for no reason the owner could act on. The platform implementation
//! lives in `brgr_platform::sleep`.

/// Holds the sleep assertion for as long as it lives.
pub(crate) type KeepAwake = brgr_platform::sleep::SleepGuard;

/// Starts holding the assertion. Best effort: a platform without the
/// mechanism simply runs as before.
pub(crate) fn keep_awake() -> KeepAwake {
    brgr_platform::sleep::keep_awake()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn the_assertion_is_held_while_the_guard_lives_and_released_with_it() {
        let guard = keep_awake();
        let pid = guard.child_pid().expect("caffeinate is part of macOS");
        assert!(
            brgr_platform::process::pid_alive(pid),
            "caffeinate should be running"
        );
        drop(guard);
        assert!(
            !brgr_platform::process::pid_alive(pid),
            "caffeinate outlived its guard"
        );
    }
}
