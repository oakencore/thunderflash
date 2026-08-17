use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

/// Socket buffer size. The defaults are tuned for the internet; this link is
/// a cable, so give the kernel room to keep it full.
pub const SOCKET_BUFFER_BYTES: libc::c_int = 8 * 1024 * 1024;

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn cstr(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL"))
}

/// Bypass the unified buffer cache for this descriptor.
///
/// Without this, a large transfer evicts everything else the machine had
/// cached and adds a memcpy per block. Unlike Linux's O_DIRECT there is no
/// alignment requirement, so this is free to turn on.
pub fn set_nocache(fd: RawFd) -> io::Result<()> {
    let rc = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) };
    if rc == -1 { Err(last_error()) } else { Ok(()) }
}

pub fn set_socket_buffers(fd: RawFd, bytes: libc::c_int) -> io::Result<()> {
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                std::ptr::addr_of!(bytes).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc == -1 {
            return Err(last_error());
        }
    }
    Ok(())
}

pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let path = cstr(path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination path is not valid UTF-8",
        )
    })?)?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd == -1 {
        Err(last_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

pub fn mkdir_at(dir: BorrowedFd, name: &str, mode: u32) -> io::Result<()> {
    let name = cstr(name)?;
    let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) };
    if rc == -1 {
        let err = last_error();
        // Already present is the normal case on a second transfer.
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(err);
        }
    }
    Ok(())
}

/// Open a subdirectory without following symlinks.
fn open_dir_at(dir: BorrowedFd, name: &str) -> io::Result<OwnedFd> {
    let name = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd == -1 {
        Err(last_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// Create and open every component in turn, returning the final directory.
///
/// This is the escape-prevention primitive. Because each `openat` carries
/// O_NOFOLLOW, a component that exists as a symlink fails with ELOOP instead
/// of silently redirecting the write outside the root. String checks in
/// `wire::sanitize` cannot catch that case, so both layers are required.
pub fn walk_dirs(root: BorrowedFd, components: &[&str]) -> io::Result<OwnedFd> {
    let mut current = root.try_clone_to_owned()?;
    for component in components {
        mkdir_at(current.as_fd(), component, 0o755)?;
        current = open_dir_at(current.as_fd(), component).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("refusing to descend into {component:?}: {err}"),
            )
        })?;
    }
    Ok(current)
}

pub fn create_file_at(dir: BorrowedFd, name: &str, mode: u32) -> io::Result<File> {
    let cname = cstr(name)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            cname.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd == -1 {
        Err(last_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub fn symlink_at(target: &str, dir: BorrowedFd, name: &str) -> io::Result<()> {
    let target = cstr(target)?;
    let name = cstr(name)?;
    let rc = unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) };
    if rc == -1 { Err(last_error()) } else { Ok(()) }
}

pub fn unlink_at(dir: BorrowedFd, name: &str) -> io::Result<()> {
    let name = cstr(name)?;
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if rc == -1 { Err(last_error()) } else { Ok(()) }
}

pub fn set_mtime_at(dir: BorrowedFd, name: &str, mtime: i64) -> io::Result<()> {
    let name = cstr(name)?;
    let times = [
        libc::timespec {
            tv_sec: mtime as libc::time_t,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: mtime as libc::time_t,
            tv_nsec: 0,
        },
    ];
    let rc = unsafe {
        libc::utimensat(
            dir.as_raw_fd(),
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == -1 { Err(last_error()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Create a unique scratch directory under the system temp dir.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tf-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walk_dirs_creates_nested_directories() {
        let root = scratch("walk-creates");
        let root_fd = open_dir(&root).unwrap();
        let dir = walk_dirs(root_fd.as_fd(), &["a", "b", "c"]).unwrap();
        let mut file = create_file_at(dir.as_fd(), "hello.txt", 0o644).unwrap();
        file.write_all(b"hi").unwrap();
        drop(file);

        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c/hello.txt")).unwrap(),
            "hi"
        );
    }

    #[test]
    fn walk_dirs_is_idempotent_over_existing_directories() {
        let root = scratch("walk-idempotent");
        let root_fd = open_dir(&root).unwrap();
        walk_dirs(root_fd.as_fd(), &["a", "b"]).unwrap();
        walk_dirs(root_fd.as_fd(), &["a", "b"]).unwrap();
        assert!(root.join("a/b").is_dir());
    }

    #[test]
    fn walk_dirs_refuses_to_traverse_a_symlink() {
        let root = scratch("walk-symlink");
        let outside = scratch("walk-symlink-outside");
        // A hostile sender's `escape -> /tmp/...outside` symlink already on disk.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let root_fd = open_dir(&root).unwrap();
        let result = walk_dirs(root_fd.as_fd(), &["escape", "loot"]);

        assert!(
            result.is_err(),
            "walk_dirs must not follow a symlinked component"
        );
        assert!(
            !outside.join("loot").exists(),
            "nothing may be created outside the root"
        );
    }

    #[test]
    fn create_file_at_refuses_to_write_through_a_symlink() {
        let root = scratch("create-symlink");
        let outside = scratch("create-symlink-outside");
        let target = outside.join("victim.txt");
        std::fs::write(&target, "original").unwrap();
        std::os::unix::fs::symlink(&target, root.join("innocent.txt")).unwrap();

        let root_fd = open_dir(&root).unwrap();
        let result = create_file_at(root_fd.as_fd(), "innocent.txt", 0o644);

        assert!(
            result.is_err(),
            "create_file_at must not write through a symlink"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    fn symlink_at_and_mkdir_at_create_entries() {
        let root = scratch("symlink-mkdir");
        let root_fd = open_dir(&root).unwrap();
        mkdir_at(root_fd.as_fd(), "sub", 0o755).unwrap();
        symlink_at("../elsewhere", root_fd.as_fd(), "link").unwrap();

        assert!(root.join("sub").is_dir());
        assert_eq!(
            std::fs::read_link(root.join("link"))
                .unwrap()
                .to_str()
                .unwrap(),
            "../elsewhere"
        );
    }

    #[test]
    fn set_mtime_at_applies_the_requested_time() {
        let root = scratch("mtime");
        let root_fd = open_dir(&root).unwrap();
        drop(create_file_at(root_fd.as_fd(), "f.txt", 0o644).unwrap());
        set_mtime_at(root_fd.as_fd(), "f.txt", 1_600_000_000).unwrap();

        let meta = std::fs::metadata(root.join("f.txt")).unwrap();
        let secs = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, 1_600_000_000);
    }

    #[test]
    fn set_nocache_succeeds_on_a_real_file() {
        let root = scratch("nocache");
        let root_fd = open_dir(&root).unwrap();
        let file = create_file_at(root_fd.as_fd(), "f.bin", 0o644).unwrap();
        set_nocache(file.as_raw_fd()).unwrap();
    }
}
