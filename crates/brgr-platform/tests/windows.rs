//! Windows versions of the grandchild-cancellation and symlink-escape tests.
//! The Unix oracle lives in `brgr-runner`'s process-group tests; this file
//! proves the same behaviour against the platform API on Windows.

#![cfg(windows)]

use std::{
    fs,
    io::Read as _,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use brgr_platform::process::{GroupSpawn, ProcessTree, pid_alive};

const HELPER_ENV: &str = "BRGR_PLATFORM_TEST_HELPER";

/// Re-executes this test binary in a helper mode so no shell or fixture is
/// needed: `sleeper` records its pid then sleeps; `parent` does the same and
/// also spawns the `sleeper` grandchild first.
fn spawn_helper(mode: &str, pid_file: &Path) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env(HELPER_ENV, mode)
        .env("BRGR_PLATFORM_TEST_PIDFILE", pid_file)
        .arg("--ignored")
        .args(["--test-threads", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .in_new_group();
    command.spawn().unwrap()
}

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{} was never written", path.display());
}

#[test]
fn cancellation_stops_a_process_tree_with_a_grandchild() {
    let dir = tempfile::tempdir().unwrap();
    let parent_pid_file = dir.path().join("parent.pid");
    let grandchild_pid_file = dir.path().join("child.pid");

    let mut parent = spawn_helper("parent", &parent_pid_file);
    let parent_pid = parent.id();
    let grandchild_pid = wait_for_pid_file(&grandchild_pid_file);
    assert!(pid_alive(parent_pid));
    assert!(pid_alive(grandchild_pid));

    let tree = ProcessTree::adopt(parent_pid).unwrap();
    tree.kill();
    let _ = parent.wait();

    let deadline = Instant::now() + Duration::from_secs(30);
    while pid_alive(grandchild_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !pid_alive(grandchild_pid),
        "grandchild {grandchild_pid} survived the tree kill"
    );
}

#[test]
fn open_no_follow_rejects_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.txt");
    fs::write(&target, b"payload").unwrap();
    let link = dir.path().join("link.txt");
    std::os::windows::fs::symlink_file(&target, &link).unwrap();

    assert!(brgr_platform::fs::open_no_follow(&link).is_err());
    let mut contents = String::new();
    brgr_platform::fs::open_no_follow(&target)
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();
    assert_eq!(contents, "payload");
}

#[test]
fn open_matching_rejects_a_symlink_after_the_metadata_check() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, b"outside").unwrap();
    let path = dir.path().join("sealed.txt");
    fs::write(&path, b"sealed").unwrap();
    let before = fs::symlink_metadata(&path).unwrap();
    // Swapping a symlink in between the stat and the open must not open it.
    fs::remove_file(&path).unwrap();
    std::os::windows::fs::symlink_file(&outside, &path).unwrap();
    assert!(brgr_platform::fs::open_matching(&path, &before).is_err());
}

/// Helper body run by every re-executed binary before any test: `sleeper`
/// records its pid and sleeps; `parent` records its own pid, spawns a
/// `sleeper` grandchild, then sleeps.
fn run_helper_mode() {
    let Ok(mode) = std::env::var(HELPER_ENV) else {
        return;
    };
    let pid_file = std::env::var("BRGR_PLATFORM_TEST_PIDFILE").unwrap();
    fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
    if mode == "parent" {
        let child_pid_file = Path::new(&pid_file).with_file_name("child.pid");
        spawn_helper("sleeper", &child_pid_file);
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[ignore = "helper mode for the re-executed test binary"]
#[test]
fn helper_entry() {
    run_helper_mode();
}
