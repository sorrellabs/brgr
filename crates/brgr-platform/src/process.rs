//! Spawning a child in its own process tree and stopping the whole tree.
//!
//! Unix groups the child with `process_group(0)` and signals the group with
//! `/bin/kill -<SIG> -- -<pgid>`; Windows puts the child in a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so `TerminateJobObject`, or simply
//! losing the handle, stops every member including grandchildren.

use std::io;

#[cfg(unix)]
mod imp {
    use std::{io, process::Command};

    use std::os::unix::process::CommandExt as _;

    /// Starts the child as the leader of a new process group, so a signal to
    /// `-pid` reaches every member the leader did not detach.
    pub fn spawn_in_new_group(command: &mut Command) -> &mut Command {
        command.process_group(0)
    }

    fn signal_group(pgid: u32, signal: &str) -> bool {
        Command::new("/bin/kill")
            .arg(format!("-{signal}"))
            .arg("--")
            .arg(format!("-{pgid}"))
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// A process tree rooted at one pid. On Unix the tree is the process
    /// group whose id is the leader's pid, so adoption is just remembering it.
    pub struct ProcessTree {
        pgid: u32,
    }

    impl ProcessTree {
        /// Adopts a child spawned with [`spawn_in_new_group`]. The pid must
        /// be the group leader's.
        ///
        /// # Errors
        ///
        /// Infallible on Unix.
        pub fn adopt(pid: u32) -> io::Result<Self> {
            Ok(Self { pgid: pid })
        }

        /// Sends `TERM` to the whole group. Returns `false` when the group is
        /// already gone, matching the `/bin/kill` exit status.
        #[must_use]
        pub fn terminate(&self) -> bool {
            signal_group(self.pgid, "TERM")
        }

        /// Sends `KILL` to the whole group, first checking with `kill -0`
        /// that any member still exists.
        pub fn kill(&self) {
            if self.is_alive() {
                let _ = signal_group(self.pgid, "KILL");
            }
        }

        /// Sends `KILL` to the whole group unconditionally. Returns `false`
        /// when the group was already gone.
        #[must_use]
        pub fn kill_now(&self) -> bool {
            signal_group(self.pgid, "KILL")
        }

        /// Whether any member of the group still exists (`kill -0`).
        #[must_use]
        pub fn is_alive(&self) -> bool {
            signal_group(self.pgid, "0")
        }

        /// A Unix tree needs no handle to stay alive; releasing is a no-op.
        pub fn release(self) {}
    }

    /// Whether a bare pid names a live process (`kill -0`).
    #[must_use]
    pub fn pid_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

#[cfg(windows)]
mod imp {
    use std::{ffi::c_void, io, mem, process::Command};

    use std::os::windows::process::CommandExt as _;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, STILL_ACTIVE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{
                CREATE_NEW_PROCESS_GROUP, GetExitCodeProcess, OpenProcess,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
            },
        },
    };

    /// Starts the child in a new process group. Tree cleanup is handled by the
    /// Job Object assigned in [`ProcessTree::adopt`]; a Job cannot be attached
    /// before the process exists, so children spawned in the narrow window
    /// between `CreateProcess` and `AssignProcessToJobObject` are missed.
    pub fn spawn_in_new_group(command: &mut Command) -> &mut Command {
        command.creation_flags(CREATE_NEW_PROCESS_GROUP)
    }

    struct Handle(*mut c_void);

    impl Handle {
        fn open(handle: *mut c_void) -> Option<Self> {
            (!handle.is_null()).then_some(Self(handle))
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a live kernel object owned by this Handle and
            // closed exactly once here.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// A Job Object owning the process tree. `KILL_ON_JOB_CLOSE` stops every
    /// member when the last handle closes, so dropping an armed tree is the
    /// emergency stop and [`ProcessTree::release`] is the deliberate disarm.
    pub struct ProcessTree {
        job: Handle,
        process: Handle,
    }

    impl ProcessTree {
        /// Adopts a running pid into a new Job Object with
        /// `KILL_ON_JOB_CLOSE`.
        ///
        /// # Errors
        ///
        /// Returns an error when the job cannot be created or the pid no
        /// longer names a process that can be opened and assigned.
        pub fn adopt(pid: u32) -> io::Result<Self> {
            // SAFETY: null attributes give the job a default security
            // descriptor and no name; the returned handle is checked below.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            let job = Handle::open(job).ok_or_else(io::Error::last_os_error)?;
            // SAFETY: zeroed is a valid initial state for the whole struct;
            // the limit flag is set next.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `job` is a valid Job handle and `info` points to a fully
            // initialised JOBOBJECT_EXTENDED_LIMIT_INFORMATION of the stated size.
            let configured = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `pid` identifies a process we are allowed to control;
            // the returned handle is checked before use.
            let process = unsafe {
                OpenProcess(
                    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    pid,
                )
            };
            let process = Handle::open(process).ok_or_else(io::Error::last_os_error)?;
            // SAFETY: both handles are valid and owned by this call.
            if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { job, process })
        }

        /// Stops every member of the tree. Windows has no `TERM`; this is
        /// already the hard stop, identical to [`ProcessTree::kill`].
        #[must_use]
        pub fn terminate(&self) -> bool {
            // SAFETY: `job` is a valid Job handle; exit code 1 is arbitrary.
            (unsafe { TerminateJobObject(self.job.0, 1) }) != 0
        }

        /// Stops every member that is still running.
        pub fn kill(&self) {
            if self.is_alive() {
                let _ = self.terminate();
            }
        }

        /// Stops every member unconditionally. Returns `false` when
        /// `TerminateJobObject` failed.
        #[must_use]
        pub fn kill_now(&self) -> bool {
            self.terminate()
        }

        /// Whether the root process is still running.
        #[must_use]
        pub fn is_alive(&self) -> bool {
            let mut member: i32 = 0;
            // SAFETY: both handles are valid; `member` is an out-param.
            let in_job = unsafe {
                IsProcessInJob(self.process.0, self.job.0, std::ptr::from_mut(&mut member))
            };
            if in_job == 0 || member == 0 {
                return false;
            }
            let mut code: u32 = 0;
            // SAFETY: `process` is a valid process handle; `code` is an out-param.
            if unsafe { GetExitCodeProcess(self.process.0, std::ptr::from_mut(&mut code)) } == 0 {
                return false;
            }
            code == STILL_ACTIVE as u32
        }

        /// Disarms the tree: the Job handle is intentionally leaked so
        /// `KILL_ON_JOB_CLOSE` does not stop the still-running children.
        /// The job itself stays alive as long as a member is assigned to it.
        #[must_use]
        pub fn release(self) {
            mem::forget(self);
        }
    }

    /// Whether a bare pid names a live process.
    #[must_use]
    pub fn pid_alive(pid: u32) -> bool {
        // SAFETY: the handle is checked and closed before returning.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        let Some(process) = Handle::open(process) else {
            return false;
        };
        let mut code: u32 = 0;
        // SAFETY: `process` is valid for this call; `code` is an out-param.
        if unsafe { GetExitCodeProcess(process.0, std::ptr::from_mut(&mut code)) } == 0 {
            return false;
        }
        code == STILL_ACTIVE as u32
    }
}

/// Sends `TERM` to a single process. Returns whether the signal was sent.
#[cfg(unix)]
#[must_use]
pub fn terminate_pid(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-TERM", "--", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Terminates a single process. Returns whether the call was issued.
#[cfg(windows)]
#[must_use]
pub fn terminate_pid(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
    };
    // SAFETY: the handle is checked and closed before returning.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: `process` is a valid handle; exit code 1 is arbitrary.
    let ok = unsafe { TerminateProcess(process, 1) } != 0;
    // SAFETY: `process` is a live handle owned by this scope.
    unsafe { CloseHandle(process) };
    ok
}

/// Sets the child's argv[0] to a custom value.
///
/// Unix: `CommandExt::arg0`. Windows cannot override argv[0] (it is derived
/// from the command line), so this is a no-op and the child sees the name it
/// was launched under; callers relying on the disguise degrade accordingly.
#[cfg(unix)]
pub fn set_argv0(command: &mut std::process::Command, argv0: impl AsRef<std::ffi::OsStr>) {
    use std::os::unix::process::CommandExt as _;
    command.arg0(argv0);
}

/// Sets the child's argv[0]; see the Unix version for the contract.
#[cfg(windows)]
pub fn set_argv0(_command: &mut std::process::Command, _argv0: impl AsRef<std::ffi::OsStr>) {}

/// Chainable form of [`spawn_in_new_group`] for `Command` builders.
pub trait GroupSpawn {
    /// Configures the command so the spawned child leads a new process tree.
    fn in_new_group(&mut self) -> &mut Self;
}

impl GroupSpawn for std::process::Command {
    fn in_new_group(&mut self) -> &mut Self {
        spawn_in_new_group(self)
    }
}

impl GroupSpawn for tokio::process::Command {
    fn in_new_group(&mut self) -> &mut Self {
        spawn_in_new_group(self.as_std_mut());
        self
    }
}

pub use imp::{ProcessTree, pid_alive, spawn_in_new_group};

/// One row of a process-table snapshot: the pid, its parent, and a command
/// name suitable for identity checks.
#[derive(Clone, Debug)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    pub command: String,
}

/// Every process with its parent and command, from one snapshot.
///
/// Unix reads `ps -A -o pid=,ppid=,comm=`; Windows walks a Toolhelp32
/// snapshot, whose `szExeFile` is the image file name rather than a full
/// command line.
#[cfg(unix)]
#[must_use]
pub fn process_table() -> Option<Vec<ProcessEntry>> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse().ok()?;
                let parent = fields.next()?.parse().ok()?;
                let command = fields.collect::<Vec<_>>().join(" ");
                Some(ProcessEntry {
                    pid,
                    ppid: parent,
                    command,
                })
            })
            .collect(),
    )
}

/// Every process with its parent and command, from one Toolhelp32 snapshot.
#[cfg(windows)]
#[must_use]
pub fn process_table() -> Option<Vec<ProcessEntry>> {
    use std::os::windows::ffi::OsStringExt as _;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: snapshotting all processes; the handle is checked and closed.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entries = Vec::new();
    // SAFETY: zeroed is a valid initial state; dwSize is set to the real size.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `entry` is a valid, correctly-sized PROCESSENTRY32W out-param
    // for the lifetime of the loop; `snapshot` stays open until it ends.
    unsafe {
        let mut more = Process32FirstW(snapshot, std::ptr::from_mut(&mut entry));
        while more != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            entries.push(ProcessEntry {
                pid: entry.th32ProcessID,
                ppid: entry.th32ParentProcessID,
                command: std::ffi::OsString::from_wide(&entry.szExeFile[..len])
                    .to_string_lossy()
                    .into_owned(),
            });
            more = Process32NextW(snapshot, std::ptr::from_mut(&mut entry));
        }
        CloseHandle(snapshot);
    }
    Some(entries)
}

/// One `ps`-style field of a single process.
///
/// On Unix this is a passthrough for `ps -ww -p <pid> -o <field>=`. On Windows
/// only `ppid`, `comm`, and `lstart` are supported: `comm` is the image file
/// name, and `lstart` is the creation `FILETIME` as a decimal string — the
/// format differs from `ps` but is stable enough to compare for equality,
/// which is all callers do.
///
/// # Errors
///
/// Returns an error when the process is not observable or the field is
/// empty (Unix) or unsupported (Windows).
#[cfg(unix)]
pub fn process_field(pid: u32, field: &str) -> io::Result<String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-ww", "-p", &pid.to_string(), "-o"])
        .arg(format!("{field}="))
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} is not observable"),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} has no {field} identity"),
        ));
    }
    Ok(text)
}

/// One `ps`-style field of a single process; see the Unix version for the
/// contract and the supported-field difference.
#[cfg(windows)]
pub fn process_field(pid: u32, field: &str) -> io::Result<String> {
    let unsupported = || {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("process field {field} is not supported on Windows"),
        )
    };
    match field {
        "ppid" | "comm" => {
            let entry = process_table()
                .and_then(|table| table.into_iter().find(|row| row.pid == pid))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("process {pid} is not observable"),
                    )
                })?;
            Ok(if field == "ppid" {
                entry.ppid.to_string()
            } else {
                entry.command
            })
        }
        "lstart" => {
            use windows_sys::Win32::{
                Foundation::{CloseHandle, FILETIME},
                System::Threading::{
                    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                },
            };
            // SAFETY: the handle is checked and closed before returning.
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if process.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: zeroed is a valid FILETIME.
            let mut creation: FILETIME = unsafe { std::mem::zeroed() };
            let mut zero: FILETIME = unsafe { std::mem::zeroed() };
            // SAFETY: `process` is a valid handle and every out-param points
            // at a live FILETIME.
            let ok = unsafe {
                GetProcessTimes(
                    process,
                    std::ptr::from_mut(&mut creation),
                    std::ptr::from_mut(&mut zero),
                    std::ptr::from_mut(&mut zero),
                    std::ptr::from_mut(&mut zero),
                )
            };
            // SAFETY: `process` is a live handle owned by this scope.
            unsafe { CloseHandle(process) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(format!(
                "{}",
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime)
            ))
        }
        _ => Err(unsupported()),
    }
}

/// The full command line of a process, when the platform exposes one.
///
/// Unix reads `ps -o command=`. Windows returns the image path via
/// `QueryFullProcessImageNameW`, which lacks arguments.
#[cfg(unix)]
#[must_use]
pub fn command_line(pid: u32) -> Option<String> {
    process_field(pid, "command").ok()
}

/// The image path of a process (arguments are not available via Toolhelp32).
#[cfg(windows)]
#[must_use]
pub fn command_line(pid: u32) -> Option<String> {
    use std::os::windows::ffi::OsStringExt as _;

    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
    };

    // SAFETY: the handle is checked and closed before returning.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buffer = [0u16; 1024];
    let mut size = buffer.len() as u32;
    // SAFETY: `process` is valid, `buffer` is a live writable region and
    // `size` is an in/out length.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process,
            0,
            buffer.as_mut_ptr(),
            std::ptr::from_mut(&mut size),
        )
    };
    // SAFETY: `process` is a live handle owned by this scope.
    unsafe { CloseHandle(process) };
    if ok == 0 {
        return None;
    }
    Some(
        std::ffi::OsString::from_wide(&buffer[..size as usize])
            .to_string_lossy()
            .into_owned(),
    )
}

/// The parent pid of a process, read from one snapshot.
#[cfg(unix)]
#[must_use]
pub fn parent_pid(pid: u32) -> Option<u32> {
    process_field(pid, "ppid").ok()?.parse().ok()
}

/// The parent pid of a process, read from one Toolhelp32 snapshot.
#[cfg(windows)]
#[must_use]
pub fn parent_pid(pid: u32) -> Option<u32> {
    process_table()?
        .into_iter()
        .find(|row| row.pid == pid)
        .map(|row| row.ppid)
}
