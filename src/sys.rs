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

/// Flush a descriptor all the way to permanent storage.
///
/// `fsync` on Darwin only hands the blocks to the drive; the drive may still
/// hold them in a volatile write cache. `F_FULLFSYNC` asks it to empty that
/// cache, which is the difference between "written" and "survives a power
/// cut". Filesystems that cannot do it report ENOTSUP/EINVAL, and there plain
/// `fsync` is the strongest thing available — say so once and carry on.
pub fn full_fsync(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) } != -1 {
        return Ok(());
    }
    let err = last_error();
    if !matches!(err.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::EINVAL)) {
        return Err(err);
    }
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "tf: this filesystem does not support F_FULLFSYNC; falling back to fsync, \
             so full durability is unavailable"
        );
    });
    let rc = unsafe { libc::fsync(fd) };
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

/// Open an existing directory chain, creating nothing.
///
/// The manifest's "do we already hold this file?" check needs the parents'
/// descriptors but must not leave a trace of a file it decides against, so
/// this is `walk_dirs` minus the mkdir. Same escape-prevention discipline:
/// every component opened O_NOFOLLOW.
pub fn open_dir_chain(root: BorrowedFd, components: &[&str]) -> io::Result<OwnedFd> {
    let mut current = root.try_clone_to_owned()?;
    for component in components {
        current = open_dir_at(current.as_fd(), component)?;
    }
    Ok(current)
}

/// Size and mtime of a regular file directly inside an open directory.
///
/// The leaf is stat'd AT_SYMLINK_NOFOLLOW, and anything that is not a plain
/// file (missing, a symlink, a directory) is an error, since the caller
/// treats every failure the same way: we do not have this, so send it.
pub fn stat_file_in_dir(dir: BorrowedFd, name: &str) -> io::Result<(u64, i64)> {
    let name = cstr(name)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == -1 {
        return Err(last_error());
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok((st.st_size as u64, st.st_mtime as i64))
}

/// Create the scratch file a payload lands in before it is renamed into place,
/// and report the name it got.
///
/// The name has to be one the sender cannot also be sending. A source tree
/// holding both `foo` and a file literally called `.foo.tf-partial` would
/// otherwise put two different inodes through the same directory entry, and
/// whichever rename ran last would decide which of the two survived — under
/// the wrong name, with every digest still matching. The random middle puts
/// the name out of a sender's reach, and O_EXCL turns the collision that is
/// left into an error instead of a silent truncation.
pub fn create_temp_file_at(dir: BorrowedFd, name: &str, mode: u32) -> io::Result<(File, String)> {
    for _ in 0..4 {
        let temp = format!(".{name}.{:08x}.tf-partial", unsafe { libc::arc4random() });
        let cname = cstr(&temp)?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                cname.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd != -1 {
            return Ok((unsafe { File::from_raw_fd(fd) }, temp));
        }
        let err = last_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(err);
        }
    }
    Err(io::Error::from_raw_os_error(libc::EEXIST))
}

pub fn create_file_at(dir: BorrowedFd, name: &str, mode: u32) -> io::Result<File> {
    let cname = cstr(name)?;
    let open = || unsafe {
        libc::openat(
            dir.as_raw_fd(),
            cname.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    let mut fd = open();
    if fd == -1 && last_error().raw_os_error() == Some(libc::EACCES) {
        // A read-only file left by an earlier transfer cannot be reopened for
        // writing. Replacing it is what a transfer means, so unlink and retry
        // once. O_NOFOLLOW means this can never be a symlink (that is ELOOP),
        // and a directory we cannot write to fails the retry too.
        if unlink_at(dir, name).is_ok() {
            fd = open();
        }
    }
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

/// Move an entry to another name in the same directory, replacing whatever is
/// there in one step. Both names are relative to `dir`, so nothing can be
/// redirected by a symlinked parent between the two.
pub fn rename_at(dir: BorrowedFd, from: &str, to: &str) -> io::Result<()> {
    let from = cstr(from)?;
    let to = cstr(to)?;
    let rc =
        unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) };
    if rc == -1 { Err(last_error()) } else { Ok(()) }
}

pub fn unlink_at(dir: BorrowedFd, name: &str) -> io::Result<()> {
    let name = cstr(name)?;
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if rc == -1 { Err(last_error()) } else { Ok(()) }
}

/// atime and mtime, both set to the same second.
fn timestamps(mtime: i64) -> [libc::timespec; 2] {
    let ts = libc::timespec {
        tv_sec: mtime as libc::time_t,
        tv_nsec: 0,
    };
    [ts, ts]
}

pub fn set_mtime_at(dir: BorrowedFd, name: &str, mtime: i64) -> io::Result<()> {
    let name = cstr(name)?;
    let times = timestamps(mtime);
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

/// Apply permission bits through an open descriptor, once the file is final.
///
/// Only the low 9 bits: the sender ships a whole `st_mode`, and setuid/setgid
/// from another machine is not something this tool grants.
pub fn set_mode_fd(fd: RawFd, mode: u32) -> io::Result<()> {
    let rc = unsafe { libc::fchmod(fd, (mode & 0o777) as libc::mode_t) };
    if rc == -1 { Err(last_error()) } else { Ok(()) }
}

/// Stamp mtime through an open descriptor.
///
/// The receiver's writer thread only knows the file is complete once it has
/// written the last byte; stamping by path from the network thread would race
/// against those writes, which bump mtime again.
pub fn set_mtime_fd(fd: RawFd, mtime: i64) -> io::Result<()> {
    let times = timestamps(mtime);
    let rc = unsafe { libc::futimens(fd, times.as_ptr()) };
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
    fn rename_at_replaces_an_existing_file() {
        let root = scratch("rename-replace");
        std::fs::write(root.join(".f.txt.tf-partial"), b"new").unwrap();
        std::fs::write(root.join("f.txt"), b"old").unwrap();

        let root_fd = open_dir(&root).unwrap();
        rename_at(root_fd.as_fd(), ".f.txt.tf-partial", "f.txt").unwrap();

        assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"new");
        assert!(!root.join(".f.txt.tf-partial").exists());
    }

    /// Replacing needs write permission on the directory, not on the file
    /// being replaced — which is why the temp-then-rename path does not need
    /// the unlink retry that opening a read-only file for writing does.
    #[test]
    fn rename_at_replaces_a_read_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("rename-readonly");
        let path = root.join("locked.bin");
        std::fs::write(root.join(".locked.bin.tf-partial"), b"new").unwrap();
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let root_fd = open_dir(&root).unwrap();
        rename_at(root_fd.as_fd(), ".locked.bin.tf-partial", "locked.bin").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn manifest_walk_and_leaf_stat_report_size_and_mtime() {
        let root = scratch("stat-regular");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/f.bin"), b"12345").unwrap();
        let root_fd = open_dir(&root).unwrap();
        set_mtime_at(
            open_dir(&root.join("a/b")).unwrap().as_fd(),
            "f.bin",
            1_600_000_000,
        )
        .unwrap();

        let parent = open_dir_chain(root_fd.as_fd(), &["a", "b"]).unwrap();
        assert_eq!(
            stat_file_in_dir(parent.as_fd(), "f.bin").unwrap(),
            (5, 1_600_000_000)
        );
    }

    /// The manifest check must never be talked into stat'ing the far end of a
    /// symlink, at the leaf or at any component along the way, and a missing
    /// parent stops the walk rather than answering from somewhere else.
    #[test]
    fn manifest_walk_refuses_symlinks_and_missing_paths() {
        let root = scratch("stat-symlink");
        let outside = scratch("stat-symlink-outside");
        std::fs::write(outside.join("victim.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.join("victim.txt"), root.join("leaf")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let root_fd = open_dir(&root).unwrap();
        assert!(open_dir_chain(root_fd.as_fd(), &["escape"]).is_err());
        assert!(open_dir_chain(root_fd.as_fd(), &["absent", "x"]).is_err());
        assert!(stat_file_in_dir(root_fd.as_fd(), "leaf").is_err());
        assert!(stat_file_in_dir(root_fd.as_fd(), "absent").is_err());
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
    fn set_mtime_fd_applies_the_requested_time_after_a_write() {
        let root = scratch("mtime-fd");
        let root_fd = open_dir(&root).unwrap();
        let mut file = create_file_at(root_fd.as_fd(), "f.txt", 0o644).unwrap();
        file.write_all(b"data").unwrap();
        set_mtime_fd(file.as_raw_fd(), 1_600_000_000).unwrap();
        drop(file);

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
    fn full_fsync_persists_a_written_file() {
        let root = scratch("fullfsync");
        let root_fd = open_dir(&root).unwrap();
        let mut file = create_file_at(root_fd.as_fd(), "f.bin", 0o644).unwrap();
        file.write_all(b"durable").unwrap();
        full_fsync(file.as_raw_fd()).unwrap();
        full_fsync(root_fd.as_raw_fd()).unwrap();

        assert_eq!(std::fs::read(root.join("f.bin")).unwrap(), b"durable");
    }

    #[test]
    fn full_fsync_reports_an_error_it_cannot_fall_back_from() {
        // A pipe is not a vnode: Darwin refuses F_FULLFSYNC on it with EBADF,
        // which is not one of the "filesystem cannot do it" codes, so it must
        // surface rather than turn into a silent fsync.
        let (_reader, writer) = std::io::pipe().unwrap();
        let err = full_fsync(writer.as_raw_fd()).unwrap_err();
        assert!(
            err.raw_os_error().is_some(),
            "expected an OS error, got {err}"
        );
    }

    /// A read-only file left by an earlier transfer cannot be reopened for
    /// writing, which used to wedge every retry into that destination.
    #[test]
    fn create_file_at_replaces_a_read_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("readonly-replace");
        let path = root.join("locked.bin");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let root_fd = open_dir(&root).unwrap();
        let mut file = create_file_at(root_fd.as_fd(), "locked.bin", 0o644).unwrap();
        file.write_all(b"new").unwrap();
        set_mode_fd(file.as_raw_fd(), 0o444).unwrap();
        drop(file);

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }

    /// The sender ships a whole st_mode; only the permission bits are applied.
    #[test]
    fn set_mode_fd_drops_type_and_setuid_bits() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("mode-fd");
        let root_fd = open_dir(&root).unwrap();
        let file = create_file_at(root_fd.as_fd(), "f.bin", 0o644).unwrap();
        set_mode_fd(file.as_raw_fd(), 0o104755).unwrap();
        drop(file);

        let mode = std::fs::metadata(root.join("f.bin"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755, "setuid must not be granted");
    }

    #[test]
    fn set_nocache_succeeds_on_a_real_file() {
        let root = scratch("nocache");
        let root_fd = open_dir(&root).unwrap();
        let file = create_file_at(root_fd.as_fd(), "f.bin", 0o644).unwrap();
        set_nocache(file.as_raw_fd()).unwrap();
    }
}
