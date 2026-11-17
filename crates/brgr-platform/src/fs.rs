//! Filesystem primitives whose meaning differs per platform.
//!
//! * Open without following a final symlink: `O_NOFOLLOW` on Unix,
//!   `FILE_FLAG_OPEN_REPARSE_POINT` plus an explicit reparse-point reject on
//!   Windows.
//! * Private modes: `0o600`/`0o700` on Unix. On Windows the default per-user
//!   ACL under the profile already restricts the file to its owner, so these
//!   are no-ops — the difference is documented here, not hidden.

use std::{fs, io, path::Path};

/// Opens a file for reading without following a symlink at its final
/// component, and without blocking on a fifo.
///
/// # Errors
///
/// Returns an error when the path cannot be opened, is a symlink (Unix
/// `O_NOFOLLOW`), or is any reparse point (Windows).
#[cfg(unix)]
pub fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Opens a file for reading while rejecting a reparse point (junction,
/// symlink, or anything else) at the final component.
#[cfg(windows)]
pub fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _};

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
    };

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION out-param.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` is an open handle and `info` is a live out-param.
    let ok = unsafe {
        GetFileInformationByHandle(file.as_raw_handle().cast(), std::ptr::from_mut(&mut info))
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{} is a reparse point", path.display()),
        ));
    }
    Ok(file)
}

/// Opens `path` and verifies the opened file is the same regular file that
/// `before` (from `fs::symlink_metadata`, taken first) described — the
/// stat-then-open race check a swapped symlink cannot defeat.
///
/// Returns `Ok(None)` when the opened file is not the described regular file.
/// On Unix the identity check is `dev`/`ino`/`len`; on Windows there is no
/// stable metadata identity, so the open itself rejects reparse points and
/// directories, which covers every substitution a symlink could perform.
///
/// # Errors
///
/// Returns the open or metadata error on Unix, and the open error on Windows.
#[cfg(unix)]
pub fn open_matching(path: &Path, before: &fs::Metadata) -> io::Result<Option<fs::File>> {
    use std::os::unix::fs::MetadataExt as _;

    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    let same = before.is_file()
        && opened.is_file()
        && before.dev() == opened.dev()
        && before.ino() == opened.ino()
        && before.len() == opened.len();
    Ok(same.then_some(file))
}

/// Opens `path` and verifies it names a regular file; see the Unix version
/// for the contract.
#[cfg(windows)]
pub fn open_matching(path: &Path, before: &fs::Metadata) -> io::Result<Option<fs::File>> {
    let _ = before;
    let file = open_no_follow(path)?;
    Ok(file.metadata()?.is_file().then_some(file))
}

/// Marks a directory private to its owner (`0o700`).
///
/// On Windows this is a deliberate no-op: a directory created under the
/// user's profile already carries a per-user ACL, which is the platform's
/// equivalent of `0o700` — there are no mode bits to set.
///
/// # Errors
///
/// Returns the `fs::set_permissions` error on Unix.
pub fn set_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

/// Marks a file private to its owner (`0o600`).
///
/// Same Windows contract as [`set_private_dir`]: the default profile ACL is
/// already owner-only.
///
/// # Errors
///
/// Returns the `fs::set_permissions` error on Unix.
pub fn set_private_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

/// Marks an already-open file private to its owner (`0o600`).
///
/// Same Windows contract as [`set_private_dir`].
///
/// # Errors
///
/// Returns the `File::set_permissions` error on Unix.
pub fn set_private_file_mode(file: &fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    let _ = file;
    Ok(())
}

/// A comparable identity for "is this still the same directory entry"
/// checks. Unix: `(dev, ino)`. Windows has no stable inode number on stable
/// Rust, so the key carries attributes, size and write time — it catches a
/// swapped or rewritten entry, short of a replace with identical metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileKey(u64, u64, u64);

#[cfg(unix)]
/// The directory-entry identity of `metadata`.
#[must_use]
pub fn file_key(metadata: &fs::Metadata) -> FileKey {
    use std::os::unix::fs::MetadataExt as _;
    FileKey(metadata.dev(), metadata.ino(), 0)
}

/// The directory-entry identity of `metadata`; see the Unix version.
#[cfg(windows)]
#[must_use]
pub fn file_key(metadata: &fs::Metadata) -> FileKey {
    use std::os::windows::fs::MetadataExt as _;
    FileKey(
        u64::from(metadata.file_attributes()),
        metadata.file_size(),
        u64::from(metadata.last_write_time()),
    )
}

/// Whether two snapshots of the same path still describe one unchanged entry:
/// same identity, same length, same timestamps.
#[cfg(unix)]
#[must_use]
pub fn unchanged(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    file_key(before) == file_key(after)
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
}

/// Whether two snapshots still describe one unchanged entry; see the Unix
/// version. `last_write_time` is part of the key already.
#[cfg(windows)]
#[must_use]
pub fn unchanged(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    file_key(before) == file_key(after) && before.len() == after.len()
}

/// The permission bits of an existing file (`mode & 0o777`), for carrying to
/// an atomic replacement. Always `None` on Windows, where there are no mode
/// bits to preserve.
#[cfg(unix)]
#[must_use]
pub fn mode_bits(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(metadata.permissions().mode() & 0o777)
}

/// The permission bits of an existing file; see the Unix version.
#[cfg(windows)]
#[must_use]
pub fn mode_bits(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

/// Mode bits to carry onto a replacement for `path`: its current mode, or
/// `fallback` when it cannot be read. `None` on Windows.
#[must_use]
pub fn preserved_mode(path: &Path, fallback: u32) -> Option<u32> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| mode_bits(&metadata))
        .or({
            #[cfg(unix)]
            {
                Some(fallback)
            }
            #[cfg(windows)]
            {
                let _ = fallback;
                None
            }
        })
}

/// Applies preserved mode bits to an open file. `None` (and Windows) is a
/// no-op.
///
/// # Errors
///
/// Returns the `File::set_permissions` error on Unix.
pub fn apply_mode_bits(file: &fs::File, mode: Option<u32>) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Some(mode) = mode {
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
    }
    #[cfg(windows)]
    let _ = (file, mode);
    Ok(())
}

/// An `OpenOptions` that creates a file private to its owner (`0o600`).
///
/// Caller still sets `.create(true)`, `.write(true)` etc. On Windows the
/// create-time mode is the default per-user ACL, so this is a plain
/// `OpenOptions`.
#[cfg(unix)]
#[must_use]
pub fn private_create_options() -> fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut options = fs::OpenOptions::new();
    options.mode(0o600);
    options
}

/// An `OpenOptions` that creates a file private to its owner; see the Unix
/// version.
#[cfg(windows)]
#[must_use]
pub fn private_create_options() -> fs::OpenOptions {
    fs::OpenOptions::new()
}

/// Whether a file is a fifo/any non-regular special type callers reject.
#[cfg(unix)]
#[must_use]
pub fn is_fifo(file_type: &fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    file_type.is_fifo()
}

/// Windows has no fifos; always `false`.
#[cfg(windows)]
#[must_use]
pub fn is_fifo(_file_type: &fs::FileType) -> bool {
    false
}

/// Whether the file at `path` may be executed.
///
/// Unix checks the owner/group/other execute bits. Windows has no mode bits;
/// a file is executable when its extension appears in `PATHEXT` (falling back
/// to the conventional `.COM;.EXE;.BAT;.CMD` when `PATHEXT` is unset).
#[cfg(unix)]
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.is_file()
        && path
            .metadata()
            .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

/// Whether the file at `path` may be executed; see the Unix version for the
/// contract.
#[cfg(windows)]
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    let pathext = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .to_uppercase();
    let wanted = format!(".{}", extension.to_uppercase());
    pathext.split(';').any(|ext| ext.trim_end() == wanted)
}
