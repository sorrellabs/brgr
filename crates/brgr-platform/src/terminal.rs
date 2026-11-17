//! Terminal lifecycle and resize notification.
//!
//! Unix delivers `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGWINCH`. Windows consoles
//! deliver ctrl-c/ctrl-break/ctrl-close instead, and there is no resize
//! signal, so the console screen buffer is polled on a helper thread and a
//! [`TermSignal::Resize`] is produced when its size changes.

use std::io;

/// A terminal event the TUI loop reacts to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TermSignal {
    Interrupt,
    Terminate,
    Hangup,
    Resize,
}

/// A bundle of terminal signals awaited as one stream.
pub struct Signals {
    #[cfg(unix)]
    inner: unix::Signals,
    #[cfg(windows)]
    inner: windows::Signals,
}

impl Signals {
    /// # Errors
    ///
    /// Returns the signal-registration error of the platform backend.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            inner: imp::Signals::new()?,
        })
    }

    /// The next terminal event. Yields only on an event.
    pub async fn next(&mut self) -> TermSignal {
        self.inner.next().await
    }
}

#[cfg(unix)]
mod imp {
    pub(super) use super::unix::Signals;
}

#[cfg(windows)]
mod imp {
    pub(super) use super::windows::Signals;
}

#[cfg(unix)]
mod unix {
    use tokio::signal::unix::{Signal, SignalKind, signal};

    use super::TermSignal;

    pub struct Signals {
        sigint: Signal,
        sigterm: Signal,
        sighup: Signal,
        sigwinch: Signal,
    }

    impl Signals {
        pub fn new() -> std::io::Result<Self> {
            Ok(Self {
                sigint: signal(SignalKind::interrupt())?,
                sigterm: signal(SignalKind::terminate())?,
                sighup: signal(SignalKind::hangup())?,
                sigwinch: signal(SignalKind::from_raw(libc::SIGWINCH))?,
            })
        }

        pub async fn next(&mut self) -> TermSignal {
            tokio::select! {
                _ = self.sigint.recv() => TermSignal::Interrupt,
                _ = self.sigterm.recv() => TermSignal::Terminate,
                _ = self.sighup.recv() => TermSignal::Hangup,
                _ = self.sigwinch.recv() => TermSignal::Resize,
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::time::Duration;

    use tokio::{
        signal::windows::{CtrlBreak, CtrlC, CtrlClose},
        time::Interval,
    };

    use super::TermSignal;

    fn console_size() -> (i16, i16) {
        use windows_sys::Win32::System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
        };
        // SAFETY: read-only query on the process's own stdout console; all
        // out-params are live and the handle is not closed.
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            // SAFETY: zeroed is a valid CONSOLE_SCREEN_BUFFER_INFO out-param.
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(handle, std::ptr::from_mut(&mut info)) == 0 {
                return (0, 0);
            }
            (info.dwSize.X, info.dwSize.Y)
        }
    }

    /// There is no console-resize signal, so `resize_poll` ticks and a size
    /// change against `last_size` produces a `Resize` event.
    pub struct Signals {
        ctrl_c: CtrlC,
        ctrl_break: CtrlBreak,
        ctrl_close: CtrlClose,
        resize_poll: Interval,
        last_size: (i16, i16),
    }

    impl Signals {
        pub fn new() -> std::io::Result<Self> {
            use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};
            Ok(Self {
                ctrl_c: ctrl_c()?,
                ctrl_break: ctrl_break()?,
                ctrl_close: ctrl_close()?,
                resize_poll: tokio::time::interval(Duration::from_millis(250)),
                last_size: console_size(),
            })
        }

        pub async fn next(&mut self) -> TermSignal {
            tokio::select! {
                _ = self.ctrl_c.recv() => TermSignal::Interrupt,
                _ = self.ctrl_break.recv() => TermSignal::Terminate,
                _ = self.ctrl_close.recv() => TermSignal::Hangup,
                _ = self.resize_poll.tick() => {
                    let size = console_size();
                    if size == self.last_size {
                        // A poll that saw no change is not an event; wait again.
                        return Box::pin(self.next()).await;
                    }
                    self.last_size = size;
                    TermSignal::Resize
                }
            }
        }
    }
}
