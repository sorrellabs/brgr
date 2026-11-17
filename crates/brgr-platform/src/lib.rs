//! Platform-specific primitives behind one internal crate.
//!
//! Every `std::os::unix`, `libc`, or Win32 call the rest of the workspace
//! needs lives here, once, with a Unix implementation (today's behaviour,
//! moved unchanged) and a Windows one (Job Objects, Toolhelp32, reparse-point
//! checks, `SetThreadExecutionState`). It is an implementation crate, not a
//! product layer: no CLI surface, nothing installed separately.
//!
//! The AGENTS.md boundaries hold the same way on both platforms: argv arrays
//! without a shell, bounded output, deadlines, and no symlink escape.

#[cfg(not(any(unix, windows)))]
compile_error!("brgr-platform supports unix and windows only");

pub mod fs;
pub mod process;
pub mod sleep;
pub mod terminal;
