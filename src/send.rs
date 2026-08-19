use std::io::{self, Read, Write};
use std::net::{SocketAddrV4, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver as Pool, SyncSender, sync_channel};
use std::time::{Instant, UNIX_EPOCH};

use crate::progress::{Diag, Progress, add_elapsed, pooled_recv};
use crate::sys;
use crate::wire::{
    ACK, CHUNK, DIGEST_LEN, Digest, Entry, FLAG_NO_VERIFY, IDLE_TIMEOUT, Kind, NOCACHE_MIN, Stats,
    TOKEN_LEN, encode_handshake, hash_update, queue_depth, read_bitmap, set_timeouts,
    write_terminator,
};

pub struct Item {
    pub abs: PathBuf,
    pub entry: Entry,
    /// The header as it travels on the wire, encoded once at the walk so the
    /// manifest and the data phase cannot drift apart and neither encodes
    /// twice.
    pub header: Vec<u8>,
}

fn make_item(abs: &Path, rel: String, kind: Kind, size: u64, mode: u32, mtime: i64) -> Item {
    let entry = Entry {
        kind,
        path: rel,
        size,
        mode,
        mtime,
    };
    // Kind + length + size + mode + mtime, then the path.
    let mut header = Vec::with_capacity(23 + entry.path.len());
    entry.encode(&mut header);
    Item {
        abs: abs.to_path_buf(),
        entry,
        header,
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    match meta.modified() {
        Ok(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(delta) => delta.as_secs() as i64,
            // Pre-epoch timestamps are rare but legal.
            Err(err) => -(err.duration().as_secs() as i64),
        },
        Err(_) => 0,
    }
}

/// Build the transfer list. Symlinks are recorded, never followed — which
/// also means no loop detection is needed.
pub fn walk(paths: &[PathBuf]) -> io::Result<Vec<Item>> {
    let mut items = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("cannot name {path:?}"))
            })?
            .to_string();
        visit(path, name, &mut items)?;
    }
    Ok(items)
}

fn visit(abs: &Path, rel: String, items: &mut Vec<Item>) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(abs)?;
    let mode = meta.permissions().mode();
    let mtime = mtime_of(&meta);

    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(abs)?;
        let target = target.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("symlink target of {abs:?} is not UTF-8"),
            )
        })?;
        items.push(make_item(
            abs,
            rel,
            Kind::Symlink,
            target.len() as u64,
            mode,
            mtime,
        ));
        return Ok(());
    }

    if meta.is_dir() {
        items.push(make_item(abs, rel.clone(), Kind::Dir, 0, mode, mtime));
        let mut children: Vec<_> = std::fs::read_dir(abs)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let name = child.file_name();
            let name = name.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{name:?} is not valid UTF-8"),
                )
            })?;
            visit(&child.path(), format!("{rel}/{name}"), items)?;
        }
        return Ok(());
    }

    // FIFOs, sockets and devices are not sendable. A FIFO would block
    // stream_file's open() forever; the rest fail or produce garbage.
    // Skipping with a warning beats a silent hang.
    if !meta.file_type().is_file() {
        eprintln!("tf: skipping {rel}: not a regular file");
        return Ok(());
    }

    items.push(make_item(abs, rel, Kind::File, meta.len(), mode, mtime));
    Ok(())
}

enum Msg {
    Header(Vec<u8>),
    /// A symlink target. Tens of bytes, sent once — not worth a pool slot.
    Target(Vec<u8>),
    /// File data in a recycled buffer. Only `len` bytes are live; the buffer
    /// itself is always CHUNK long and goes back to the pool after the write.
    Chunk {
        buf: Vec<u8>,
        len: usize,
    },
    /// BLAKE3 of the payload just sent. Queued rather than written directly so
    /// it lands after that payload's last chunk.
    Digest(Digest),
    /// Reader marking the end of a file's chunks. The hash stage turns it into
    /// a `Digest`; nothing downstream of that stage ever sees it.
    EndPayload,
}

/// Middle stage: hashes each chunk between the reader and the socket writer,
/// so disk reads, hashing and socket writes overlap instead of taking turns.
/// The hash state itself stays on this one thread (order is order), but the
/// compression work inside `hash_update` spreads across cores for buffers at
/// or above `RAYON_MIN`. Buffers pass straight through; the pool still bounds
/// memory.
fn hash_items(rx: Pool<io::Result<Msg>>, tx: SyncSender<io::Result<Msg>>, diag: Option<Arc<Diag>>) {
    let diag = diag.as_deref();
    let mut hasher = blake3::Hasher::new();

    for message in rx {
        let forward = match message {
            Ok(Msg::Chunk { buf, len }) => {
                let started = Instant::now();
                hash_update(&mut hasher, &buf[..len]);
                if let Some(diag) = diag {
                    add_elapsed(&diag.hash_nanos, started);
                }
                Ok(Msg::Chunk { buf, len })
            }
            // Empty files finalize an untouched hasher, which is exactly the
            // digest of empty input.
            Ok(Msg::EndPayload) => {
                let started = Instant::now();
                let digest = *hasher.finalize().as_bytes();
                hasher.reset();
                if let Some(diag) = diag {
                    add_elapsed(&diag.hash_nanos, started);
                }
                Ok(Msg::Digest(digest))
            }
            other => other,
        };
        if tx.send(forward).is_err() {
            return;
        }
    }
}

/// Connect, hand over every item, and wait for the receiver's ack.
pub fn send_to(
    peer: SocketAddrV4,
    paths: &[PathBuf],
    token: &[u8; TOKEN_LEN],
    progress: &Progress,
) -> io::Result<Stats> {
    send_to_diag(peer, paths, token, progress, true, None)
}

/// `send_to` with verification opt-out and diagnostics attached. With
/// `verify` false the hashing stage is not spawned at all and no digest ever
/// reaches the wire; the receiver learns this from the handshake flags.
pub fn send_to_diag(
    peer: SocketAddrV4,
    paths: &[PathBuf],
    token: &[u8; TOKEN_LEN],
    progress: &Progress,
    verify: bool,
    diag: Option<Arc<Diag>>,
) -> io::Result<Stats> {
    let started = Instant::now();
    let items = walk(paths)?;
    if let Some(diag) = &diag {
        add_elapsed(&diag.walk_nanos, started);
    }

    let mut stream = TcpStream::connect(peer)?;
    sys::set_socket_buffers(stream.as_raw_fd(), sys::SOCKET_BUFFER_BYTES)?;
    stream.set_nodelay(true)?;
    // A receiver that wedges or vanishes half-open sends no RST and starts no
    // retransmit timer, so the ACK wait at the end would otherwise be
    // unbounded.
    set_timeouts(&stream, IDLE_TIMEOUT)?;

    let flags = if verify { 0 } else { FLAG_NO_VERIFY };
    stream.write_all(&encode_handshake(flags, token))?;

    // Manifest phase. Every header travels first, in one write, so the
    // receiver can answer with the files it already holds. Only File entries
    // are offered: a directory is EEXIST-tolerant on the far side and a
    // symlink target is smaller than the bit that would describe it.
    let mut manifest = Vec::new();
    for item in &items {
        manifest.extend_from_slice(&item.header);
    }
    write_terminator(&mut manifest)?;
    stream.write_all(&manifest)?;
    stream.flush()?;
    let offered = items.iter().filter(|i| i.entry.kind == Kind::File).count();
    let have = read_bitmap(&mut stream, offered)?;

    let (items, mut stats) = keep_unskipped(items, &have);
    // Totals only now: the bar would otherwise promise bytes the manifest has
    // already established will never move.
    let total_bytes: u64 = items.iter().map(|i| i.entry.size).sum();
    progress.set_totals(items.len() as u64, total_bytes);

    let depth = queue_depth();
    // One buffer per queue slot, plus one being filled by the reader and one
    // being written to the socket. Any fewer and the pipeline stalls on
    // itself; any more is memory doing nothing.
    let (pool_tx, pool_rx) = sync_channel::<Vec<u8>>(depth + 2);
    for _ in 0..depth + 2 {
        pool_tx
            .send(vec![0u8; CHUNK])
            .expect("pool channel has room for its own prefill");
    }

    let reader_progress = progress.clone();
    let reader_diag = diag.clone();
    // read -> hash -> socket. Reads happen on their own thread so disk and
    // network overlap instead of taking turns, and hashing gets its own so it
    // does not eat into either.
    let (read_tx, read_rx) = sync_channel::<io::Result<Msg>>(depth);
    let reader_verify = verify;
    let reader = std::thread::spawn(move || {
        read_items(
            items,
            read_tx,
            pool_rx,
            reader_progress,
            reader_diag,
            reader_verify,
        )
    });
    // Without verification there is nothing to hash, so the stage is not
    // spawned at all and the reader feeds the socket directly.
    let (rx, hasher) = if verify {
        let (tx, rx) = sync_channel::<io::Result<Msg>>(depth);
        let hash_diag = diag.clone();
        (
            rx,
            Some(std::thread::spawn(move || {
                hash_items(read_rx, tx, hash_diag)
            })),
        )
    } else {
        (read_rx, None)
    };

    // Teardown is deadlock-free in both directions. If this loop leaves early
    // (socket error, or `message?`), `rx` and `pool_tx` are both dropped on
    // the way out, so the reader's next `tx.send` or `pool.recv` fails and it
    // returns. If the reader leaves first it drops `tx`, this loop sees the
    // channel close, and we fall through to the join. Neither side can be
    // left blocked on a channel the other still holds.
    for message in rx {
        match message? {
            Msg::Header(bytes) => {
                stream.write_all(&bytes)?;
                stats.files += 1;
                if let Some(diag) = &diag {
                    diag.socket_bytes.fetch_add(bytes.len() as u64, Relaxed);
                }
            }
            Msg::Target(bytes) => {
                stream.write_all(&bytes)?;
                stats.bytes += bytes.len() as u64;
                progress.add_bytes(bytes.len() as u64);
                if let Some(diag) = &diag {
                    diag.socket_bytes.fetch_add(bytes.len() as u64, Relaxed);
                }
            }
            Msg::Chunk { buf, len } => {
                stream.write_all(&buf[..len])?;
                stats.bytes += len as u64;
                progress.add_bytes(len as u64);
                if let Some(diag) = &diag {
                    diag.socket_bytes.fetch_add(len as u64, Relaxed);
                    diag.dequeued();
                }
                // Reader is blocked waiting for this; failure just means it
                // already finished and dropped the pool.
                let _ = pool_tx.send(buf);
            }
            Msg::Digest(bytes) => {
                stream.write_all(&bytes)?;
                if let Some(diag) = &diag {
                    diag.socket_bytes.fetch_add(DIGEST_LEN as u64, Relaxed);
                }
            }
            // The hash stage replaced it with a Digest.
            Msg::EndPayload => unreachable!("consumed by the hash stage"),
        }
    }
    // Both joins before any `?`, so a reader panic cannot detach the hasher.
    let read = reader.join();
    let hashed = hasher.map(|h| h.join());
    read.map_err(|_| io::Error::other("reader thread panicked"))?;
    if let Some(Err(_)) = hashed {
        return Err(io::Error::other("hash thread panicked"));
    }

    write_terminator(&mut stream)?;
    stream.flush()?;

    // Do not report success until the receiver says every byte landed.
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack)?;
    if ack[0] != ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver did not acknowledge",
        ));
    }
    Ok(stats)
}

/// Drop the files the receiver says it already has, counting them as skipped.
/// Bits line up with File entries in wire order — the only entries that were
/// offered — so anything else passes through untouched.
fn keep_unskipped(items: Vec<Item>, have: &[bool]) -> (Vec<Item>, Stats) {
    let mut stats = Stats::default();
    let mut bits = have.iter();
    let kept = items
        .into_iter()
        .filter(|item| {
            if item.entry.kind != Kind::File {
                return true;
            }
            // A short answer leaves the remainder unskipped: slower, never
            // wrong.
            if bits.next() == Some(&true) {
                stats.skipped_files += 1;
                stats.skipped_bytes += item.entry.size;
                return false;
            }
            true
        })
        .collect();
    (kept, stats)
}

fn read_items(
    items: Vec<Item>,
    tx: SyncSender<io::Result<Msg>>,
    pool: Pool<Vec<u8>>,
    progress: Progress,
    diag: Option<Arc<Diag>>,
    verify: bool,
) {
    let diag = diag.as_deref();
    for item in items {
        let Item { abs, entry, header } = item;
        progress.set_current(&entry.path);

        // The header left the walk already encoded.
        if tx.send(Ok(Msg::Header(header))).is_err() {
            return;
        }
        if let Some(diag) = diag {
            match entry.kind {
                Kind::Dir => &diag.dirs,
                Kind::Symlink => &diag.symlinks,
                Kind::File | Kind::End => &diag.files,
            }
            .fetch_add(1, Relaxed);
        }

        let result = match entry.kind {
            // Directories are header-only.
            Kind::Dir | Kind::End => Ok(()),
            Kind::Symlink => match std::fs::read_link(&abs) {
                Ok(target) => {
                    let bytes = target.to_string_lossy().into_owned().into_bytes();
                    // The header announcing this length is already on the
                    // wire. A link repointed since the walk would leave the
                    // receiver framing the next entry from the wrong offset,
                    // so abort here the way a shrinking file does.
                    if bytes.len() as u64 != entry.size {
                        let _ = tx.send(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{} changed during transfer: announced {} bytes, target is now {}",
                                entry.path,
                                entry.size,
                                bytes.len()
                            ),
                        )));
                        return;
                    }
                    if let Some(diag) = diag {
                        diag.disk_bytes.fetch_add(bytes.len() as u64, Relaxed);
                    }
                    let digest = verify.then(|| {
                        let hash_started = Instant::now();
                        let digest = crate::wire::digest(&bytes);
                        if let Some(diag) = diag {
                            add_elapsed(&diag.hash_nanos, hash_started);
                        }
                        digest
                    });
                    let sent = tx.send(Ok(Msg::Target(bytes))).and_then(|()| match digest {
                        Some(digest) => tx.send(Ok(Msg::Digest(digest))),
                        None => Ok(()),
                    });
                    match sent {
                        Ok(()) => Ok(()),
                        // Receiver hung up; the writer will surface the error.
                        Err(_) => return,
                    }
                }
                Err(err) => Err(err),
            },
            Kind::File => stream_file(&abs, &entry, &tx, &pool, diag, verify),
        };

        if let Err(err) = result {
            let _ = tx.send(Err(err));
            return;
        }
        progress.finish_file();
    }
}

fn stream_file(
    abs: &Path,
    entry: &Entry,
    tx: &SyncSender<io::Result<Msg>>,
    pool: &Pool<Vec<u8>>,
    diag: Option<&Diag>,
    verify: bool,
) -> io::Result<()> {
    let mut file = std::fs::File::open(abs)?;
    // Keep a huge transfer from evicting the machine's entire page cache.
    // Below NOCACHE_MIN the fcntl would cost more, per file, than the cache
    // it protects.
    if entry.size >= NOCACHE_MIN {
        let _ = sys::set_nocache(file.as_raw_fd());
    }

    let mut sent = 0u64;
    while sent < entry.size {
        // Blocking here is the real backpressure: no free buffer means the
        // link has not drained what we already read.
        let Some(mut buf) = pooled_recv(pool, diag) else {
            // Writer is gone; it already has the error worth reporting.
            return Ok(());
        };
        // Never read past the size we announced, so a file that grew mid
        // transfer still matches its header.
        let want = (entry.size - sent).min(CHUNK as u64) as usize;
        let mut filled = 0;
        while filled < want {
            match file.read(&mut buf[filled..want]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        if filled < want {
            // Padding the gap would deliver a file whose contents never
            // existed. Abort instead; the receiver deletes its partial.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} shrank during transfer: expected {} bytes, read {}",
                    entry.path,
                    entry.size,
                    sent + filled as u64
                ),
            ));
        }
        sent += filled as u64;
        if let Some(diag) = diag {
            diag.disk_bytes.fetch_add(filled as u64, Relaxed);
            diag.queued();
        }
        if tx.send(Ok(Msg::Chunk { buf, len: filled })).is_err() {
            return Ok(());
        }
    }
    // The hash stage turns this into the digest. Empty files reach it too, so
    // every File entry on the wire is followed by exactly one digest.
    if verify {
        let _ = tx.send(Ok(Msg::EndPayload));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Kind;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tf-send-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walk_records_files_dirs_and_symlinks_without_following() {
        let root = scratch("walk");
        std::fs::create_dir_all(root.join("tree/nested")).unwrap();
        std::fs::create_dir_all(root.join("tree/empty")).unwrap();
        std::fs::write(root.join("tree/nested/a.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("nested/a.txt", root.join("tree/link")).unwrap();

        let items = walk(&[root.join("tree")]).unwrap();
        let mut paths: Vec<(String, Kind)> = items
            .iter()
            .map(|i| (i.entry.path.clone(), i.entry.kind))
            .collect();
        paths.sort();

        assert_eq!(
            paths,
            vec![
                ("tree".to_string(), Kind::Dir),
                ("tree/empty".to_string(), Kind::Dir),
                ("tree/link".to_string(), Kind::Symlink),
                ("tree/nested".to_string(), Kind::Dir),
                ("tree/nested/a.txt".to_string(), Kind::File),
            ]
        );

        let file = items
            .iter()
            .find(|i| i.entry.path == "tree/nested/a.txt")
            .unwrap();
        assert_eq!(file.entry.size, 5);

        let link = items.iter().find(|i| i.entry.path == "tree/link").unwrap();
        assert_eq!(link.entry.size, "nested/a.txt".len() as u64);
    }

    #[test]
    fn walk_records_a_single_file_by_its_basename() {
        let root = scratch("single");
        std::fs::write(root.join("solo.bin"), b"xyz").unwrap();

        let items = walk(&[root.join("solo.bin")]).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].entry.path, "solo.bin");
        assert_eq!(items[0].entry.kind, Kind::File);
    }

    /// Drives stream_file with an entry that claims more bytes than the file
    /// holds — the same state a file that shrinks mid-transfer leaves behind.
    fn stream_with_claimed_size(name: &str, contents: &[u8], claimed: u64) -> io::Result<Vec<u8>> {
        let root = scratch(name);
        let abs = root.join("f.bin");
        std::fs::write(&abs, contents).unwrap();
        let item = Item {
            abs,
            entry: Entry {
                kind: Kind::File,
                path: "f.bin".into(),
                size: claimed,
                mode: 0o644,
                mtime: 0,
            },
            header: Vec::new(),
        };
        let (tx, rx) = sync_channel::<io::Result<Msg>>(64);
        let (pool_tx, pool_rx) = sync_channel::<Vec<u8>>(64);
        for _ in 0..8 {
            pool_tx.send(vec![0u8; CHUNK]).unwrap();
        }
        let result = stream_file(&item.abs, &item.entry, &tx, &pool_rx, None, true);
        drop(tx);
        // Recycle so the pool never starves this single-threaded drive.
        let mut got = Vec::new();
        for message in rx {
            if let Ok(Msg::Chunk { buf, len }) = message {
                got.extend_from_slice(&buf[..len]);
                let _ = pool_tx.send(buf);
            }
        }
        result.map(|()| got)
    }

    #[test]
    fn a_shrinking_file_errors_instead_of_padding_with_zeros() {
        let err = stream_with_claimed_size("shrink", b"only nine", 9_000).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("shrank"), "got: {err}");
    }

    #[test]
    fn a_file_that_grew_sends_exactly_the_recorded_size() {
        let sent = stream_with_claimed_size("grew", b"0123456789", 4).unwrap();
        assert_eq!(sent, b"0123");
    }

    #[test]
    fn a_multi_chunk_file_streams_every_byte_in_order() {
        let data: Vec<u8> = (0..CHUNK * 2 + 7).map(|i| (i % 251) as u8).collect();
        let sent = stream_with_claimed_size("multichunk", &data, data.len() as u64).unwrap();
        assert_eq!(sent, data);
    }

    /// The header announcing a symlink's target length is on the wire before
    /// the target is read, so a link repointed in between must abort the send
    /// rather than desynchronise the receiver's framing.
    #[test]
    fn a_symlink_that_changed_since_the_walk_aborts_the_send() {
        let root = scratch("link-changed");
        let link = root.join("l");
        std::os::unix::fs::symlink("short", &link).unwrap();
        let items = walk(std::slice::from_ref(&link)).unwrap();

        // Repoint it at a target of a different length, as a racing process
        // would between the walk and the read.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("a-much-longer-target", &link).unwrap();

        let (tx, rx) = sync_channel::<io::Result<Msg>>(8);
        let (_pool_tx, pool_rx) = sync_channel::<Vec<u8>>(1);
        read_items(items, tx, pool_rx, Progress::new("sending"), None, true);

        let err = rx
            .into_iter()
            .find_map(|m| m.err())
            .expect("a changed symlink must produce an error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("changed during transfer"), "{err}");
    }

    fn item(kind: Kind, path: &str, size: u64) -> Item {
        make_item(Path::new(path), path.to_string(), kind, size, 0o644, 0)
    }

    fn kept_paths(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.entry.path.as_str()).collect()
    }

    #[test]
    fn the_walk_encoded_header_matches_the_entry_on_the_wire() {
        for (kind, path, size) in [
            (Kind::File, "a/b.bin", 12u64),
            (Kind::Dir, "a", 0),
            (Kind::Symlink, "l", 4096),
        ] {
            let got = make_item(Path::new(path), path.to_string(), kind, size, 0o644, 0);
            let mut want = Vec::new();
            got.entry.encode(&mut want);
            assert_eq!(got.header, want, "mismatch for {path:?}");
        }
    }

    #[test]
    fn the_bitmap_drops_only_the_files_the_receiver_already_has() {
        let items = vec![
            item(Kind::File, "a", 10),
            item(Kind::File, "b", 20),
            item(Kind::File, "c", 30),
        ];
        let (kept, stats) = keep_unskipped(items, &[true, false, true]);

        assert_eq!(kept_paths(&kept), vec!["b"]);
        assert_eq!(stats.skipped_files, 2);
        assert_eq!(stats.skipped_bytes, 40);
    }

    /// Dirs and symlinks are never offered, so they consume no bits and must
    /// survive however the file bits fall.
    #[test]
    fn dirs_and_symlinks_are_kept_and_consume_no_bits() {
        let items = vec![
            item(Kind::Dir, "d", 0),
            item(Kind::File, "d/a", 7),
            item(Kind::Symlink, "d/l", 3),
            item(Kind::File, "d/b", 9),
        ];
        let (kept, stats) = keep_unskipped(items, &[true, false]);

        assert_eq!(kept_paths(&kept), vec!["d", "d/l", "d/b"]);
        assert_eq!(stats.skipped_files, 1);
        assert_eq!(stats.skipped_bytes, 7);
    }

    #[test]
    fn an_all_zero_bitmap_keeps_everything() {
        let items = vec![item(Kind::File, "a", 1), item(Kind::File, "b", 2)];
        let (kept, stats) = keep_unskipped(items, &[false, false]);

        assert_eq!(kept_paths(&kept), vec!["a", "b"]);
        assert_eq!(stats, Stats::default());
    }

    #[test]
    fn walk_skips_special_files_instead_of_hanging_on_them() {
        let root = scratch("special");
        std::fs::write(root.join("real.txt"), b"ok").unwrap();
        let fifo = std::ffi::CString::new(root.join("pipe").to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        let items = walk(std::slice::from_ref(&root)).unwrap();

        assert!(
            items.iter().any(|i| i.entry.path.ends_with("real.txt")),
            "regular files must still be walked"
        );
        assert!(
            !items.iter().any(|i| i.entry.path.ends_with("pipe")),
            "FIFOs must be skipped, got: {:?}",
            items.iter().map(|i| &i.entry.path).collect::<Vec<_>>()
        );
    }
}
