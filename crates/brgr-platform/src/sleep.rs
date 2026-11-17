//! Keeps the machine from idle-sleeping during one slow operation.
//!
//! macOS spawns `caffeinate -i -w <pid>` so the assertion dies with this
//! process even if brgr is killed. Windows raises an execution-state
//! requirement with `SetThreadExecutionState` and releases it on drop. Other
//! platforms are a no-op.

/// Holds the sleep assertion for as long as it lives.
pub struct SleepGuard {
    #[cfg(target_os = "macos")]
    child: Option<std::process::Child>,
    #[cfg(windows)]
    _private: (),
}

/// Starts holding the assertion. Best effort: a platform or machine without
/// the mechanism simply runs as before.
#[cfg(target_os = "macos")]
#[must_use]
pub fn keep_awake() -> SleepGuard {
    use std::process::{Command, Stdio};
    let child = Command::new("/usr/bin/caffeinate")
        .args(["-i", "-w", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
    SleepGuard { child }
}

/// Starts holding the assertion; see the macOS version for the contract.
#[cfg(windows)]
#[must_use]
pub fn keep_awake() -> SleepGuard {
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    // SAFETY: process-wide power hint; every state transition is paired by
    // the matching call in Drop.
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
    }
    SleepGuard { _private: () }
}

/// No sleep assertion exists on this platform; always a no-op guard.
#[cfg(not(any(target_os = "macos", windows)))]
#[must_use]
pub fn keep_awake() -> SleepGuard {
    SleepGuard {}
}

#[cfg(target_os = "macos")]
impl SleepGuard {
    /// The pid of the `caffeinate` child holding the assertion, for tests.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(std::process::Child::id)
    }
}

#[cfg(target_os = "macos")]
impl Drop for SleepGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(windows)]
impl Drop for SleepGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Power::{ES_CONTINUOUS, SetThreadExecutionState};
        // SAFETY: releases the requirement raised in keep_awake; ES_CONTINUOUS
        // with no other flag clears it.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}
