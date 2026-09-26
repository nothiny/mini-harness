use std::path::Path;

#[cfg(all(unix, not(target_os = "linux")))]
use std::path::Component;

pub(super) async fn replace_file(root: &Path, from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let root = root.to_owned();
        let from = from.to_owned();
        let to = to.to_owned();
        tokio::task::spawn_blocking(move || replace_file_linux(&root, &from, &to))
            .await
            .map_err(|error| std::io::Error::other(format!("replace task failed: {error}")))?
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let root = root.to_owned();
        let from = from.to_owned();
        let to = to.to_owned();
        tokio::task::spawn_blocking(move || replace_file_unix(&root, &from, &to))
            .await
            .map_err(|error| std::io::Error::other(format!("replace task failed: {error}")))?
    }
    #[cfg(windows)]
    {
        let _ = root;
        let from = from.to_owned();
        let to = to.to_owned();
        tokio::task::spawn_blocking(move || replace_file_windows(&from, &to))
            .await
            .map_err(|error| std::io::Error::other(format!("replace task failed: {error}")))?
    }
}

/// Opens a canonical workspace path while keeping path traversal below the
/// canonical workspace root. On Linux this uses `openat2(2)` so a concurrent
/// symlink or mount-point swap cannot redirect the operation outside the root.
/// Other platforms retain the canonical-path check and reject a symlink at the
/// final component where the platform exposes that flag.
pub(crate) async fn open_workspace_file(
    root: &Path,
    path: &Path,
) -> std::io::Result<tokio::fs::File> {
    let root = root.to_owned();
    let path = path.to_owned();
    let file = tokio::task::spawn_blocking(move || open_workspace_file_blocking(&root, &path))
        .await
        .map_err(|error| std::io::Error::other(format!("secure open task failed: {error}")))??;
    Ok(tokio::fs::File::from_std(file))
}

fn open_workspace_file_blocking(_root: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        open_file_linux(_root, path)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        open_file_unix(_root, path)
    }
    #[cfg(windows)]
    {
        std::fs::File::open(path)
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_file_unix(root: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    let root = std::fs::canonicalize(root)?;
    let relative = path.strip_prefix(&root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is outside the workspace root",
        )
    })?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let name = relative.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "file path has no name")
    })?;
    let directory = open_directory_unix(&root, parent)?;
    let name = CString::new(name.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "file path contains NUL")
    })?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_directory_unix(root: &Path, relative: &Path) -> std::io::Result<std::fs::File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    let root_name = CString::new(root.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace path contains NUL",
        )
    })?;
    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut directory = unsafe { std::fs::File::from_raw_fd(root_fd) };
    for component in relative.components() {
        let name = match component {
            Component::Normal(name) => name,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "directory path contains a non-normal component",
                ));
            }
        };
        let name = CString::new(name.as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory path contains NUL",
            )
        })?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn replace_file_unix(root: &Path, from: &Path, to: &Path) -> std::io::Result<()> {
    use std::{
        ffi::CString,
        os::{fd::AsRawFd, unix::ffi::OsStrExt},
    };

    let from_parent = from.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "temporary file has no parent",
        )
    })?;
    let to_parent = to.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target file has no parent",
        )
    })?;
    if from_parent != to_parent {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "temporary file and target have different parents",
        ));
    }
    let root = std::fs::canonicalize(root)?;
    let relative_parent = to_parent.strip_prefix(&root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target parent is outside the workspace root",
        )
    })?;
    let directory = open_directory_unix(&root, relative_parent)?;
    let from_name = CString::new(
        from.file_name()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "temporary file has no name",
                )
            })?
            .as_bytes(),
    )
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name contains NUL"))?;
    let to_name = CString::new(
        to.file_name()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "target file has no name")
            })?
            .as_bytes(),
    )
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name contains NUL"))?;
    let result = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            from_name.as_ptr(),
            directory.as_raw_fd(),
            to_name.as_ptr(),
        )
    };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn open_file_linux(root: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    let root = std::fs::canonicalize(root)?;
    let relative = path.strip_prefix(&root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is outside the workspace root",
        )
    })?;
    let root_name = CString::new(root.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace path contains NUL",
        )
    })?;
    let relative_name = CString::new(relative.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "file path contains NUL")
    })?;

    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let root_file = unsafe { std::fs::File::from_raw_fd(root_fd) };
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root_file.as_raw_fd(),
            relative_name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd as i32) })
}

#[cfg(target_os = "linux")]
fn replace_file_linux(root: &Path, from: &Path, to: &Path) -> std::io::Result<()> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };

    let from_parent = from.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "temporary file has no parent",
        )
    })?;
    let to_parent = to.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target file has no parent",
        )
    })?;
    if from_parent != to_parent {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "temporary file and target have different parents",
        ));
    }
    let root = std::fs::canonicalize(root)?;
    let relative_parent = to_parent.strip_prefix(&root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target parent is outside the workspace root",
        )
    })?;
    let root_name = CString::new(root.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace path contains NUL",
        )
    })?;
    let from_name = CString::new(
        from.file_name()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "temporary file has no name",
                )
            })?
            .as_bytes(),
    )
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name contains NUL"))?;
    let to_name = CString::new(
        to.file_name()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "target file has no name")
            })?
            .as_bytes(),
    )
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name contains NUL"))?;

    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let root_file = unsafe { std::fs::File::from_raw_fd(root_fd) };
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
    };
    // `openat2` rejects an empty path, but a target directly inside the
    // workspace root has no relative parent. Reuse the root descriptor then.
    let parent_file = if relative_parent.as_os_str().is_empty() {
        root_file.try_clone()?
    } else {
        let parent_name = CString::new(relative_parent.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "parent path contains NUL")
        })?;
        let parent_fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root_file.as_raw_fd(),
                parent_name.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if parent_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { std::fs::File::from_raw_fd(parent_fd as i32) }
    };
    let result = unsafe {
        libc::renameat(
            parent_file.as_raw_fd(),
            from_name.as_ptr(),
            parent_file.as_raw_fd(),
            to_name.as_ptr(),
        )
    };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[cfg(target_os = "linux")]
const RESOLVE_BENEATH: u64 = 0x08;
#[cfg(target_os = "linux")]
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;

#[cfg(windows)]
fn replace_file_windows(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex_w(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    let existing: Vec<u16> = from.as_os_str().encode_wide().chain([0]).collect();
    let replacement: Vec<u16> = to.as_os_str().encode_wide().chain([0]).collect();
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let success = unsafe {
        move_file_ex_w(
            existing.as_ptr(),
            replacement.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
