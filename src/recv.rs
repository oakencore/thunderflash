use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver as ChanReceiver, SyncSender, sync_channel};
use std::time::Instant;

use crate::progress::{Diag, Progress, add_elapsed, pooled_recv};
use crate::sys;
use crate::wire::{
    ACK, CHUNK, DIGEST_LEN, Digest, Entry, FLAG_NO_VERIFY, HANDSHAKE_LEN, HANDSHAKE_TIMEOUT,
    IDLE_TIMEOUT, Kind, Stats, TOKEN_LEN, ct_eq, decode_handshake, queue_depth, sanitize,
    set_timeouts,
};

/// Work handed to the writer thread. `Begin` carries the descriptors the
/// writer needs to finish *and* to clean up after itself: the open file, and a
/// share of its parent directory's descriptor so it can unlink by name without
/// a path.
enum Msg {
    Begin(InFlight),
    Data {
        buf: Vec<u8>,
        len: usize,
    },
    EndFile,
    /// Net thread asking the hash stage for the digest of the payload it has
    /// forwarded so far. Consumed by `hash_loop`; the writer never sees it.
    Finish(SyncSender<Digest>),
}

/// A destination file that is not yet known good, and its own undo.
///
/// The unlink lives in `Drop` rather than in each error path because this
/// value travels: it is created by the network thread, queued through the hash
/// stage, and only then owned by the writer. Anywhere it is dropped without
/// being completed — a `SendError` payload when a stage has died, an error
/// return, an unwind — the half-written file goes with it. Exactly one party
/// unlinks, because exactly one party holds the value.
struct InFlight {
    file: File,
    parent: Arc<OwnedFd>,
    name: String,
    mtime: i64,
    /// Sender-supplied permission bits, applied once the contents are final.
    mode: u32,
    /// Set only when the file is complete: mtime stamped, and flushed if the
    /// session is durable.
    finished: bool,
}

impl Drop for InFlight {
    /// A partial file that looks complete is worse than no file at all.
    fn drop(&mut self) {
        if !self.finished {
            let _ = sys::unlink_at(self.parent.as_fd(), &self.name);
        }
    }
}

/// Drain the queue onto disk. Runs on one thread for the whole session.
///
/// Ends the file's life in every direction: normal completion stamps mtime
/// through the descriptor, a write error unlinks and reports, and the channel
/// closing mid-file (sender hung up) unlinks too. Exactly one party unlinks,
/// and it is always this one.
///
/// With `durable`, a file is not complete until it has also been flushed to
/// permanent storage; a failed flush is as fatal as a failed write.
fn write_loop(
    rx: ChanReceiver<Msg>,
    pool: SyncSender<Vec<u8>>,
    diag: Option<Arc<Diag>>,
    durable: bool,
) -> io::Result<()> {
    let mut current: Option<InFlight> = None;
    let diag = diag.as_deref();

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Begin(open) => current = Some(open),
            Msg::Data { buf, len } => {
                if let Some(diag) = diag {
                    diag.dequeued();
                }
                let Some(open) = current.as_mut() else {
                    let _ = pool.send(buf);
                    continue;
                };
                let result = open.file.write_all(&buf[..len]);
                if let (Ok(()), Some(diag)) = (&result, diag) {
                    diag.disk_bytes.fetch_add(len as u64, Relaxed);
                }
                // Recycle before reacting, so a failure cannot leak a buffer.
                let _ = pool.send(buf);
                if let Err(err) = result {
                    // Dropping the in-flight file unlinks it; dropping the
                    // receiver is how the network thread learns to stop.
                    drop(current.take());
                    drop(rx);
                    return Err(err);
                }
            }
            Msg::EndFile => {
                if let Some(mut open) = current.take() {
                    // The mode is applied only now: creating with it would
                    // make a read-only file unwritable to our own writer.
                    let _ = sys::set_mode_fd(open.file.as_raw_fd(), open.mode);
                    let _ = sys::set_mtime_fd(open.file.as_raw_fd(), open.mtime);
                    if durable {
                        // After the stamp, so the mtime is what gets persisted.
                        let started = Instant::now();
                        let result = sys::full_fsync(open.file.as_raw_fd());
                        if let Some(diag) = diag {
                            add_elapsed(&diag.flush_nanos, started);
                        }
                        if let Err(err) = result {
                            // Unflushed is incomplete: treat it like a write
                            // error, so the drop below still unlinks.
                            drop(open);
                            drop(rx);
                            return Err(err);
                        }
                    }
                    open.finished = true;
                }
            }
            // The hash stage answers these; none is ever forwarded here.
            Msg::Finish(_) => {}
        }
    }

    // Channel closed mid-file (sender hung up): the drop unlinks the partial.
    drop(current.take());
    Ok(())
}

/// Middle stage: hashes each buffer on its way from the network thread to the
/// writer, so socket reads, hashing and disk writes all overlap. BLAKE3 is
/// sequential over one file, so this thread is the receiver's hashing budget.
///
/// It only computes; the network thread compares and decides, which keeps the
/// unlink contract exactly where it was — the writer, and only the writer,
/// removes a file that never got its `EndFile`.
fn hash_loop(rx: ChanReceiver<Msg>, tx: SyncSender<Msg>, diag: Option<Arc<Diag>>) {
    let diag = diag.as_deref();
    let mut hasher = blake3::Hasher::new();

    while let Ok(msg) = rx.recv() {
        let forward = match msg {
            Msg::Data { buf, len } => {
                let started = Instant::now();
                hasher.update(&buf[..len]);
                if let Some(diag) = diag {
                    add_elapsed(&diag.hash_nanos, started);
                }
                Msg::Data { buf, len }
            }
            Msg::Finish(reply) => {
                let started = Instant::now();
                let digest = *hasher.finalize().as_bytes();
                hasher.reset();
                if let Some(diag) = diag {
                    add_elapsed(&diag.hash_nanos, started);
                }
                // A dead net thread means the session is already failing.
                if reply.send(digest).is_err() {
                    return;
                }
                continue;
            }
            other => other,
        };
        // Writer is gone: stop, which closes the net thread's channel too.
        if tx.send(forward).is_err() {
            return;
        }
    }
}

pub struct Receiver {
    listener: TcpListener,
    dest: PathBuf,
    token: [u8; TOKEN_LEN],
    diag: Option<Arc<Diag>>,
    durable: bool,
}

impl Receiver {
    pub fn bind(
        dest: &Path,
        addr: Ipv4Addr,
        port: u16,
        token: [u8; TOKEN_LEN],
    ) -> io::Result<Receiver> {
        std::fs::create_dir_all(dest)?;
        let listener = TcpListener::bind((addr, port))?;
        Ok(Receiver {
            listener,
            dest: dest.to_path_buf(),
            token,
            diag: None,
            durable: false,
        })
    }

    /// Collect diagnostics for the session this receiver accepts.
    pub fn with_diag(mut self, diag: Arc<Diag>) -> Receiver {
        self.diag = Some(diag);
        self
    }

    /// Flush every file and the destination directory to permanent storage
    /// before acknowledging the transfer. Costs throughput; see the README.
    pub fn with_durable(mut self, durable: bool) -> Receiver {
        self.durable = durable;
        self
    }

    pub fn port(&self) -> io::Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    /// Accept exactly one sender, receive everything it offers, then return.
    pub fn accept_one(&self, progress: &Progress) -> io::Result<Stats> {
        let waiting = Instant::now();
        let (mut stream, peer) = self.listener.accept()?;
        if let Some(diag) = self.diag.as_deref() {
            // Idle time, not transfer time: `--stats` subtracts it.
            add_elapsed(&diag.accept_nanos, waiting);
        }
        sys::set_socket_buffers(stream.as_raw_fd(), sys::SOCKET_BUFFER_BYTES)?;
        // A peer that connects and then says nothing must not pin the only
        // session this process will ever accept. Both loops treat any read or
        // write error as fatal-and-clean-up, so a timeout needs no new path.
        set_timeouts(&stream, HANDSHAKE_TIMEOUT)?;
        let verify = self.verify_handshake(&mut stream, peer)?;
        set_timeouts(&stream, IDLE_TIMEOUT)?;
        self.receive_entries(&mut stream, progress, verify)
    }

    /// Returns whether the sender's stream carries digests to check.
    fn verify_handshake(
        &self,
        stream: &mut TcpStream,
        peer: std::net::SocketAddr,
    ) -> io::Result<bool> {
        let mut buf = [0u8; HANDSHAKE_LEN];
        stream.read_exact(&mut buf)?;
        let (flags, token) = decode_handshake(&buf)?;
        if !ct_eq(&token, &self.token) {
            eprintln!("refused a connection from {peer}: token mismatch");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "token mismatch",
            ));
        }
        let verify = flags & FLAG_NO_VERIFY == 0;
        if !verify {
            eprintln!("tf: content verification disabled by sender");
        }
        Ok(verify)
    }

    fn receive_entries(
        &self,
        stream: &mut TcpStream,
        progress: &Progress,
        verify: bool,
    ) -> io::Result<Stats> {
        let depth = queue_depth();
        let (tx, rx) = sync_channel::<Msg>(depth);
        let (pool_tx, pool_rx) = sync_channel::<Vec<u8>>(depth);
        for _ in 0..depth {
            pool_tx
                .send(vec![0u8; CHUNK])
                .expect("pool receiver is alive");
        }
        let writer_diag = self.diag.clone();
        let durable = self.durable;
        let writer = std::thread::spawn(move || write_loop(rx, pool_tx, writer_diag, durable));
        // net -> hash -> write: the buffers pass through, still bounded by the
        // same pool, so memory is unchanged and the three stages overlap.
        // A no-verify stream has nothing to hash, so the stage is skipped and
        // the net thread feeds the writer directly.
        let (entry_tx, hasher) = if verify {
            let (hash_tx, hash_rx) = sync_channel::<Msg>(depth);
            let hash_diag = self.diag.clone();
            (
                hash_tx,
                Some(std::thread::spawn(move || {
                    hash_loop(hash_rx, tx, hash_diag)
                })),
            )
        } else {
            (tx, None)
        };

        let received = self.read_entries(stream, &entry_tx, &pool_rx, progress, verify);
        drop(entry_tx);
        // Both joins happen before any `?`: returning early on a hash-thread
        // panic would leave the writer detached, still holding a file it is
        // the only party allowed to unlink.
        let hashed = hasher.map(|h| h.join());
        let written = writer.join();
        // A writer error explains any send failure the read loop saw, so it
        // wins; only then does the read loop's own error matter.
        written.map_err(|_| io::Error::other("receiver writer thread panicked"))??;
        if let Some(Err(_)) = hashed {
            return Err(io::Error::other("receiver hash thread panicked"));
        }
        let stats = received?;

        // Every file is already flushed by here; the names themselves live in
        // the directory, which needs its own flush to survive a power cut.
        if self.durable {
            let started = Instant::now();
            let root = sys::open_dir(&self.dest)?;
            sys::full_fsync(root.as_raw_fd())?;
            if let Some(diag) = self.diag.as_deref() {
                add_elapsed(&diag.flush_nanos, started);
            }
        }

        // Only now is every byte committed, so it is safe to let the sender
        // claim success.
        stream.write_all(&[ACK])?;
        stream.flush()?;
        Ok(stats)
    }

    fn read_entries(
        &self,
        stream: &mut TcpStream,
        tx: &SyncSender<Msg>,
        pool: &ChanReceiver<Vec<u8>>,
        progress: &Progress,
        verify: bool,
    ) -> io::Result<Stats> {
        let root = sys::open_dir(&self.dest)?;
        let mut stats = Stats::default();
        let diag = self.diag.as_deref();
        let sink = Sink { tx, pool, diag };
        let mut parent_cache = ParentCache::default();

        loop {
            let Some(entry) = Entry::decode(stream)? else {
                break;
            };

            let meta_started = Instant::now();
            let components = sanitize(&entry.path).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refusing path {:?}: {err:?}", entry.path),
                )
            })?;
            let (name, parents) = components
                .split_last()
                .expect("sanitize rejects empty paths");
            let name = *name;

            // walk_dirs opens each component with O_NOFOLLOW, so a symlinked
            // parent aborts here rather than redirecting the write.
            let dir = parent_cache.get(root.as_fd(), parents)?;
            if let Some(diag) = diag {
                add_elapsed(&diag.meta_nanos, meta_started);
            }

            progress.set_current(&entry.path);

            match entry.kind {
                Kind::Dir => {
                    let meta_started = Instant::now();
                    // Owner rwx regardless of what the sender asked for: a
                    // 0555 source directory would otherwise make every file
                    // inside it fail with EACCES, on this transfer and every
                    // retry. ponytail: the sender's exact directory mode is
                    // therefore not reproduced; a deferred chmod pass after
                    // the tree is complete would be the upgrade.
                    sys::mkdir_at(dir.as_fd(), name, entry.mode | 0o700)?;
                    // Files are stamped by the writer once their last byte lands.
                    let _ = sys::set_mtime_at(dir.as_fd(), name, entry.mtime);
                    if let Some(diag) = diag {
                        add_elapsed(&diag.meta_nanos, meta_started);
                        diag.dirs.fetch_add(1, Relaxed);
                    }
                }
                Kind::Symlink => {
                    let mut target = vec![0u8; entry.size as usize];
                    stream.read_exact(&mut target)?;
                    // Tens of bytes: hashed here rather than routed through the
                    // stage. Verified before the link exists, so a bad one is
                    // never created rather than created and removed.
                    if verify {
                        let started = Instant::now();
                        let actual = crate::wire::digest(&target);
                        if let Some(diag) = diag {
                            add_elapsed(&diag.hash_nanos, started);
                        }
                        check_digest(stream, actual, &entry.path, diag)?;
                    }
                    let target = String::from_utf8(target).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "symlink target is not UTF-8")
                    })?;
                    let meta_started = Instant::now();
                    // Replace any existing entry so repeat transfers work.
                    let _ = sys::unlink_at(dir.as_fd(), name);
                    sys::symlink_at(&target, dir.as_fd(), name)?;
                    // Counted so both ends agree on the byte total; the
                    // sender puts the target on the wire the same way.
                    stats.bytes += entry.size;
                    if let Some(diag) = diag {
                        add_elapsed(&diag.meta_nanos, meta_started);
                        diag.symlinks.fetch_add(1, Relaxed);
                        diag.socket_bytes.fetch_add(entry.size, Relaxed);
                    }
                }
                Kind::File => {
                    pump_file(stream, &dir, name, &entry, &sink, progress, verify)?;
                    stats.bytes += entry.size;
                    if let Some(diag) = diag {
                        diag.files.fetch_add(1, Relaxed);
                    }
                }
                Kind::End => unreachable!("decode returns None for the terminator"),
            }

            stats.files += 1;
            progress.finish_file();
        }
        Ok(stats)
    }
}

/// The directory the previous entry lived in, kept open.
///
/// Entries arrive depth-first sorted, so consecutive entries nearly always
/// share a parent and `walk_dirs` — an mkdirat plus an openat per component,
/// plus a dup of the root — would redo identical work for each one.
///
/// SAFETY (escape prevention): the cached descriptor was obtained by
/// `walk_dirs`, i.e. by opening every component with O_NOFOLLOW, so it can
/// only name a directory that was reachable without traversing a symlink. A
/// descriptor pins that directory *inode*: nothing a sender does later — not
/// replacing the name with a symlink, not renaming an ancestor — can make it
/// resolve anywhere else. Against that swap the cache is stronger than
/// re-walking the textual path; against a *local* process renaming the
/// directory out of the destination it is weaker, since a fresh walk would
/// land in the recreated path while the cached fd follows the inode. Neither
/// is a defence the sender can defeat, and a local process able to rename
/// inside the destination can already write there directly.
/// The cache is only ever consulted for a byte-identical component list, and
/// holds exactly one entry by construction, so a different parent always
/// forces a fresh O_NOFOLLOW walk.
#[derive(Default)]
struct ParentCache {
    /// Owned because the components borrow the entry, which does not outlive
    /// the iteration.
    components: Vec<String>,
    dir: Option<Arc<OwnedFd>>,
}

impl ParentCache {
    fn get(&mut self, root: BorrowedFd, parents: &[&str]) -> io::Result<Arc<OwnedFd>> {
        if let Some(dir) = &self.dir
            && self.components.len() == parents.len()
            && self.components.iter().zip(parents).all(|(a, b)| a == b)
        {
            return Ok(Arc::clone(dir));
        }
        let dir = Arc::new(sys::walk_dirs(root, parents)?);
        self.components.clear();
        self.components
            .extend(parents.iter().map(|p| p.to_string()));
        self.dir = Some(Arc::clone(&dir));
        Ok(dir)
    }
}

/// The write path as the network thread sees it: where buffers come from,
/// where filled ones go, and what to count on the way past.
struct Sink<'a> {
    tx: &'a SyncSender<Msg>,
    pool: &'a ChanReceiver<Vec<u8>>,
    diag: Option<&'a Diag>,
}

/// Placeholder: the writer's own error replaces this on join.
fn writer_gone() -> io::Error {
    io::Error::other("receiver writer thread stopped")
}

/// Read the digest that follows a payload and compare it with the one the hash
/// stage computed. A truncated stream fails here as `UnexpectedEof` rather
/// than blocking.
///
/// The caller must return this error WITHOUT sending `EndFile`: the writer
/// unlinks whatever file is still in flight when the channel closes, so
/// exactly one party removes a file that failed verification.
fn check_digest(
    stream: &mut TcpStream,
    actual: Digest,
    path: &str,
    diag: Option<&Diag>,
) -> io::Result<()> {
    let mut expected = [0u8; DIGEST_LEN];
    stream.read_exact(&mut expected)?;
    let matched = blake3::Hash::from(actual) == blake3::Hash::from(expected);
    if let Some(diag) = diag {
        diag.socket_bytes.fetch_add(DIGEST_LEN as u64, Relaxed);
    }
    if !matched {
        // Never quote the bytes: the path is all the operator needs.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path} failed content verification"),
        ));
    }
    Ok(())
}

/// Read one file's payload off the socket into pooled buffers and hand them to
/// the writer. Never touches the file's contents itself.
fn pump_file(
    stream: &mut TcpStream,
    dir: &Arc<OwnedFd>,
    name: &str,
    entry: &Entry,
    sink: &Sink,
    progress: &Progress,
    verify: bool,
) -> io::Result<()> {
    let meta_started = Instant::now();
    // Created writable whatever the sender's mode says; the writer applies
    // the real permission bits once the contents are final.
    let file = sys::create_file_at(dir.as_fd(), name, entry.mode | 0o600)?;
    // Advisory only, and non-fatal on the sender too.
    let _ = sys::set_nocache(file.as_raw_fd());
    if let Some(diag) = sink.diag {
        add_elapsed(&diag.meta_nanos, meta_started);
    }
    // From here the file exists on disk, so it is owned by an InFlight: every
    // way out of this function drops one, and dropping one unlinks.
    let open = InFlight {
        file,
        // An Arc clone, not a dup: the writer only needs *a* handle on the
        // directory to unlink by name, and this one is already open.
        parent: Arc::clone(dir),
        name: name.to_string(),
        mtime: entry.mtime,
        mode: entry.mode,
        finished: false,
    };
    if sink.tx.send(Msg::Begin(open)).is_err() {
        return Err(writer_gone());
    }

    let mut remaining = entry.size;
    while remaining > 0 {
        // Blocks here once every buffer is queued: that is the backpressure.
        let Some(mut buf) = pooled_recv(sink.pool, sink.diag) else {
            return Err(writer_gone());
        };
        let want = remaining.min(buf.len() as u64) as usize;
        let read = loop {
            match stream.read(&mut buf[..want]) {
                Ok(n) => break n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        };
        if read == 0 {
            // Connection dropped mid-file. Dropping our sender is what tells
            // the writer to unlink the partial.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "{} truncated: expected {} bytes, got {}",
                    entry.path,
                    entry.size,
                    entry.size - remaining
                ),
            ));
        }
        if let Some(diag) = sink.diag {
            diag.socket_bytes.fetch_add(read as u64, Relaxed);
            diag.queued();
        }
        if sink.tx.send(Msg::Data { buf, len: read }).is_err() {
            return Err(writer_gone());
        }
        remaining -= read as u64;
        progress.add_bytes(read as u64);
    }

    if verify {
        // The only point where the net thread waits for the hash stage: one
        // chunk's worth of catch-up at the end of each file.
        let (reply_tx, reply_rx) = sync_channel::<Digest>(1);
        if sink.tx.send(Msg::Finish(reply_tx)).is_err() {
            return Err(writer_gone());
        }
        let Ok(actual) = reply_rx.recv() else {
            return Err(writer_gone());
        };
        // Verified before EndFile, so a file that fails is left in flight for
        // the writer to unlink when the channel closes.
        check_digest(stream, actual, &entry.path, sink.diag)?;
    }

    if sink.tx.send(Msg::EndFile).is_err() {
        return Err(writer_gone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Entry, Kind};
    use std::io::Write;
    use std::net::TcpStream;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tf-recv-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn handshake(token: &[u8; TOKEN_LEN]) -> [u8; HANDSHAKE_LEN] {
        crate::wire::encode_handshake(0, token)
    }

    #[test]
    fn rejects_a_bad_token() {
        let dest = scratch("bad-token");
        let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, [7u8; TOKEN_LEN]).unwrap();
        let port = receiver.port().unwrap();

        let handle = std::thread::spawn(move || {
            let progress = Progress::new("receiving");
            receiver.accept_one(&progress)
        });

        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.write_all(&handshake(&[9u8; TOKEN_LEN])).unwrap();
        let mut entry = Vec::new();
        Entry {
            kind: Kind::File,
            path: "loot.txt".into(),
            size: 3,
            mode: 0o644,
            mtime: 0,
        }
        .encode(&mut entry);
        entry.extend_from_slice(b"abc");
        let _ = stream.write_all(&entry);
        drop(stream);

        assert!(
            handle.join().unwrap().is_err(),
            "a bad token must be refused"
        );
        assert!(!dest.join("loot.txt").exists(), "no file may be written");
    }

    #[test]
    fn refuses_a_symlink_used_to_escape_the_destination() {
        let dest = scratch("escape");
        let outside = scratch("escape-outside");
        let token = [3u8; TOKEN_LEN];

        let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
        let port = receiver.port().unwrap();
        let handle = std::thread::spawn(move || {
            let progress = Progress::new("receiving");
            receiver.accept_one(&progress)
        });

        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.write_all(&handshake(&token)).unwrap();

        // A symlink pointing outside, then a write "through" it. Both paths
        // pass every string-level rule.
        let target = outside.to_str().unwrap().to_string();
        let mut buf = Vec::new();
        Entry {
            kind: Kind::Symlink,
            path: "x".into(),
            size: target.len() as u64,
            mode: 0o777,
            mtime: 0,
        }
        .encode(&mut buf);
        buf.extend_from_slice(target.as_bytes());
        buf.extend_from_slice(&crate::wire::digest(target.as_bytes()));
        Entry {
            kind: Kind::File,
            path: "x/escaped.txt".into(),
            size: 5,
            mode: 0o644,
            mtime: 0,
        }
        .encode(&mut buf);
        buf.extend_from_slice(b"pwned");
        buf.extend_from_slice(&crate::wire::digest(b"pwned"));
        let _ = stream.write_all(&buf);
        drop(stream);

        let _ = handle.join().unwrap();
        assert!(
            !outside.join("escaped.txt").exists(),
            "receiver must not write outside the destination directory"
        );
    }

    fn begin(file: File, root: &OwnedFd, name: &str) -> Msg {
        Msg::Begin(InFlight {
            file,
            parent: Arc::new(root.try_clone().unwrap()),
            name: name.to_string(),
            mtime: 0,
            mode: 0o644,
            finished: false,
        })
    }

    /// Drive `write_loop` directly: no socket, no `Receiver`.
    fn spawn_writer(
        depth: usize,
        buf_len: usize,
        durable: bool,
    ) -> (
        SyncSender<Msg>,
        ChanReceiver<Vec<u8>>,
        std::thread::JoinHandle<io::Result<()>>,
    ) {
        let (tx, rx) = sync_channel::<Msg>(depth);
        let (pool_tx, pool_rx) = sync_channel::<Vec<u8>>(depth);
        for _ in 0..depth {
            pool_tx.send(vec![0u8; buf_len]).unwrap();
        }
        (
            tx,
            pool_rx,
            std::thread::spawn(move || write_loop(rx, pool_tx, None, durable)),
        )
    }

    #[test]
    fn a_failed_durable_flush_removes_the_file_and_fails_the_transfer() {
        let dest = scratch("flush-fail");
        let root = sys::open_dir(&dest).unwrap();
        let path = dest.join("unflushable.bin");
        std::fs::write(&path, b"placeholder").unwrap();

        let (tx, _pool, handle) = spawn_writer(1, 8, true);
        let (_reader, pipe) = std::io::pipe().unwrap();
        // A pipe cannot be flushed to storage, so EndFile fails on it.
        tx.send(begin(
            std::fs::File::from(std::os::fd::OwnedFd::from(pipe)),
            &root,
            "unflushable.bin",
        ))
        .unwrap();
        let _ = tx.send(Msg::EndFile);

        let err = handle.join().unwrap().unwrap_err();
        assert!(
            err.raw_os_error().is_some(),
            "expected an OS error, got {err}"
        );
        assert!(!path.exists(), "an unflushed file must not survive");
        assert!(
            tx.send(Msg::EndFile).is_err(),
            "the writer must drop its receiver so the network thread stops"
        );
    }

    /// The window between `create_file_at` and the writer accepting the file:
    /// if `Begin` never arrives, the file it names must not survive. The
    /// unlink is in `InFlight::drop`, so it fires even from inside the
    /// `SendError` payload of a channel whose receiver is already gone.
    #[test]
    fn a_begin_the_writer_never_receives_removes_the_file() {
        let dest = scratch("orphan-begin");
        let root = sys::open_dir(&dest).unwrap();
        let (tx, rx) = sync_channel::<Msg>(1);
        drop(rx); // the writer died on an earlier file

        let file = sys::create_file_at(root.as_fd(), "orphan.bin", 0o644).unwrap();
        assert!(dest.join("orphan.bin").exists(), "setup: file must exist");

        assert!(
            tx.send(begin(file, &root, "orphan.bin")).is_err(),
            "the writer is gone, so the send must fail"
        );
        assert!(
            !dest.join("orphan.bin").exists(),
            "a file the writer never took ownership of must not be left behind"
        );
    }

    /// The sender's mode is applied after the last byte, not at creation, so
    /// the writer can always write and a read-only file still lands read-only.
    #[test]
    fn a_read_only_mode_is_applied_after_the_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dest = scratch("mode-after");
        let root = sys::open_dir(&dest).unwrap();
        let (tx, pool, handle) = spawn_writer(2, 8, false);

        let mut open = match begin(
            sys::create_file_at(root.as_fd(), "ro.bin", 0o444 | 0o600).unwrap(),
            &root,
            "ro.bin",
        ) {
            Msg::Begin(open) => open,
            _ => unreachable!(),
        };
        open.mode = 0o444;
        tx.send(Msg::Begin(open)).unwrap();
        let mut buf = pool.recv().unwrap();
        buf.fill(7);
        tx.send(Msg::Data { buf, len: 8 }).unwrap();
        tx.send(Msg::EndFile).unwrap();
        drop(tx);
        handle.join().unwrap().unwrap();

        let meta = std::fs::metadata(dest.join("ro.bin")).unwrap();
        assert_eq!(meta.len(), 8, "the writer must have written first");
        assert_eq!(meta.permissions().mode() & 0o777, 0o444);
    }

    #[test]
    fn a_slow_writer_blocks_the_producer_instead_of_growing_the_pool() {
        // A pipe is a file the test can throttle: each write only completes
        // once the reader on the far end drains it.
        const MSG: usize = 256 * 1024; // larger than any macOS pipe buffer
        const MSGS: usize = 4;
        let sleep = std::time::Duration::from_millis(40);

        let (mut reader, writer) = std::io::pipe().unwrap();
        let drain = std::thread::spawn(move || {
            let mut sunk = Vec::new();
            let mut buf = vec![0u8; MSG];
            loop {
                std::thread::sleep(sleep);
                match reader.read(&mut buf) {
                    Ok(0) => return sunk,
                    Ok(n) => sunk.extend_from_slice(&buf[..n]),
                    Err(err) => panic!("pipe read failed: {err}"),
                }
            }
        });

        let dest = scratch("backpressure");
        let root = sys::open_dir(&dest).unwrap();
        let (tx, pool, handle) = spawn_writer(1, MSG, false);
        let started = std::time::Instant::now();

        tx.send(begin(
            std::fs::File::from(std::os::fd::OwnedFd::from(writer)),
            &root,
            "pipe",
        ))
        .unwrap();

        let mut addresses = Vec::new();
        for byte in 0..MSGS as u8 {
            let mut buf = pool.recv().unwrap();
            buf.fill(byte);
            addresses.push(buf.as_ptr());
            tx.send(Msg::Data { buf, len: MSG }).unwrap();
        }
        tx.send(Msg::EndFile).unwrap();
        drop(tx);
        handle.join().unwrap().unwrap();
        let elapsed = started.elapsed();

        assert!(
            addresses.windows(2).all(|w| w[0] == w[1]),
            "the pool must recycle one buffer, saw {} distinct allocations",
            addresses.len()
        );
        assert!(
            elapsed >= sleep * MSGS as u32,
            "producer did not block on the slow writer: {elapsed:?}"
        );
        let sunk = drain.join().unwrap();
        assert_eq!(sunk.len(), MSG * MSGS);
        assert_eq!(sunk[0], 0);
        assert_eq!(sunk[MSG * MSGS - 1], (MSGS - 1) as u8);
    }

    #[test]
    fn a_failed_write_removes_the_partial_file_and_reports_the_error() {
        let dest = scratch("write-fail");
        let root = sys::open_dir(&dest).unwrap();
        let path = dest.join("doomed.bin");
        std::fs::write(&path, b"placeholder").unwrap();

        let (tx, pool, handle) = spawn_writer(2, 16, false);
        // Read-only, so the writer's first write fails with EBADF.
        tx.send(begin(
            std::fs::File::open(&path).unwrap(),
            &root,
            "doomed.bin",
        ))
        .unwrap();
        let buf = pool.recv().unwrap();
        let _ = tx.send(Msg::Data { buf, len: 16 });

        let err = handle.join().unwrap().unwrap_err();
        assert!(
            err.raw_os_error().is_some(),
            "expected an OS error, got {err}"
        );
        assert!(!path.exists(), "the partial file must be removed");
        assert!(
            tx.send(Msg::EndFile).is_err(),
            "the writer must drop its receiver so the network thread stops"
        );
    }

    #[test]
    fn closing_the_channel_mid_file_removes_the_partial_file() {
        let dest = scratch("mid-file-close");
        let root = sys::open_dir(&dest).unwrap();
        let (tx, pool, handle) = spawn_writer(2, 8, false);

        tx.send(begin(
            sys::create_file_at(root.as_fd(), "half.bin", 0o644).unwrap(),
            &root,
            "half.bin",
        ))
        .unwrap();
        let buf = pool.recv().unwrap();
        tx.send(Msg::Data { buf, len: 8 }).unwrap();
        drop(tx); // sender hung up: no EndFile ever arrives

        handle.join().unwrap().unwrap();
        assert!(
            !dest.join("half.bin").exists(),
            "an unfinished file must not survive"
        );
    }

    #[test]
    fn deletes_a_truncated_file_rather_than_keeping_a_partial() {
        let dest = scratch("truncated");
        let token = [5u8; TOKEN_LEN];
        let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
        let port = receiver.port().unwrap();
        let handle = std::thread::spawn(move || {
            let progress = Progress::new("receiving");
            receiver.accept_one(&progress)
        });

        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.write_all(&handshake(&token)).unwrap();
        let mut buf = Vec::new();
        Entry {
            kind: Kind::File,
            path: "half.bin".into(),
            size: 1000,
            mode: 0o644,
            mtime: 0,
        }
        .encode(&mut buf);
        buf.extend_from_slice(&[0u8; 400]);
        let _ = stream.write_all(&buf);
        drop(stream); // hang up mid-file

        assert!(handle.join().unwrap().is_err());
        assert!(
            !dest.join("half.bin").exists(),
            "partial file must be removed"
        );
    }
}
