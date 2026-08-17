use std::io::{self, Read, Write};
use std::net::{SocketAddrV4, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::UNIX_EPOCH;

use crate::progress::Progress;
use crate::sys;
use crate::wire::{ACK, CHUNK, Entry, Kind, MAGIC, Stats, TOKEN_LEN, VERSION, write_terminator};

/// How many chunks may sit between the reader thread and the socket writer.
/// Bounds memory at CHUNK * this, and provides backpressure when the disk
/// outruns the link.
const QUEUE_DEPTH: usize = 4;

pub struct Item {
    pub abs: PathBuf,
    pub entry: Entry,
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
        items.push(Item {
            abs: abs.to_path_buf(),
            entry: Entry {
                kind: Kind::Symlink,
                path: rel,
                size: target.len() as u64,
                mode,
                mtime,
                offset: 0,
            },
        });
        return Ok(());
    }

    if meta.is_dir() {
        items.push(Item {
            abs: abs.to_path_buf(),
            entry: Entry {
                kind: Kind::Dir,
                path: rel.clone(),
                size: 0,
                mode,
                mtime,
                offset: 0,
            },
        });
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

    items.push(Item {
        abs: abs.to_path_buf(),
        entry: Entry {
            kind: Kind::File,
            path: rel,
            size: meta.len(),
            mode,
            mtime,
            offset: 0,
        },
    });
    Ok(())
}

enum Msg {
    Header(Vec<u8>),
    Chunk(Vec<u8>),
}

/// Connect, hand over every item, and wait for the receiver's ack.
pub fn send_to(
    peer: SocketAddrV4,
    paths: &[PathBuf],
    token: &[u8; TOKEN_LEN],
    progress: &Progress,
) -> io::Result<Stats> {
    let items = walk(paths)?;
    let total_bytes: u64 = items.iter().map(|i| i.entry.size).sum();
    progress.set_totals(items.len() as u64, total_bytes);

    let mut stream = TcpStream::connect(peer)?;
    sys::set_socket_buffers(stream.as_raw_fd(), sys::SOCKET_BUFFER_BYTES)?;
    stream.set_nodelay(true)?;

    let mut handshake = Vec::with_capacity(5 + TOKEN_LEN);
    handshake.extend_from_slice(&MAGIC.to_le_bytes());
    handshake.push(VERSION);
    handshake.extend_from_slice(token);
    stream.write_all(&handshake)?;

    let (tx, rx) = sync_channel::<io::Result<Msg>>(QUEUE_DEPTH);
    let reader_progress = progress.clone();
    // Reads happen on their own thread so disk and network overlap instead
    // of taking turns.
    let reader = std::thread::spawn(move || read_items(items, tx, reader_progress));

    let mut stats = Stats::default();
    for message in rx {
        match message? {
            Msg::Header(bytes) => {
                stream.write_all(&bytes)?;
                stats.files += 1;
            }
            Msg::Chunk(bytes) => {
                stream.write_all(&bytes)?;
                stats.bytes += bytes.len() as u64;
                progress.add_bytes(bytes.len() as u64);
            }
        }
    }
    reader
        .join()
        .map_err(|_| io::Error::other("reader thread panicked"))?;

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

fn read_items(items: Vec<Item>, tx: SyncSender<io::Result<Msg>>, progress: Progress) {
    for item in items {
        progress.set_current(&item.entry.path);

        let mut header = Vec::with_capacity(64 + item.entry.path.len());
        item.entry.encode(&mut header);
        if tx.send(Ok(Msg::Header(header))).is_err() {
            return;
        }

        let result = match item.entry.kind {
            // Directories are header-only.
            Kind::Dir | Kind::End => Ok(()),
            Kind::Symlink => match std::fs::read_link(&item.abs) {
                Ok(target) => {
                    let bytes = target.to_string_lossy().into_owned().into_bytes();
                    match tx.send(Ok(Msg::Chunk(bytes))) {
                        Ok(()) => Ok(()),
                        // Receiver hung up; the writer will surface the error.
                        Err(_) => return,
                    }
                }
                Err(err) => Err(err),
            },
            Kind::File => stream_file(&item, &tx),
        };

        if let Err(err) = result {
            let _ = tx.send(Err(err));
            return;
        }
        progress.finish_file();
    }
}

fn stream_file(item: &Item, tx: &SyncSender<io::Result<Msg>>) -> io::Result<()> {
    let mut file = std::fs::File::open(&item.abs)?;
    // Keep a huge transfer from evicting the machine's entire page cache.
    let _ = sys::set_nocache(file.as_raw_fd());

    let mut remaining = item.entry.size;
    while remaining > 0 {
        let want = remaining.min(CHUNK as u64) as usize;
        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        if filled == 0 {
            // The file shrank after we sized it. Pad so the receiver's
            // length accounting still balances.
            buf.truncate(want);
            buf.iter_mut().for_each(|b| *b = 0);
            filled = want;
        }
        buf.truncate(filled);
        remaining -= filled as u64;
        if tx.send(Ok(Msg::Chunk(buf))).is_err() {
            return Ok(());
        }
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

    #[test]
    fn walk_skips_special_files_instead_of_hanging_on_them() {
        let root = scratch("special");
        std::fs::write(root.join("real.txt"), b"ok").unwrap();
        let fifo = std::ffi::CString::new(root.join("pipe").to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        let items = walk(&[root.clone()]).unwrap();

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
