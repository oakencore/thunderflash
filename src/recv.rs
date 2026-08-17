use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};

use crate::progress::Progress;
use crate::sys;
use crate::wire::{ACK, CHUNK, Entry, Kind, MAGIC, Stats, TOKEN_LEN, VERSION, ct_eq, sanitize};

pub struct Receiver {
    listener: TcpListener,
    dest: PathBuf,
    token: [u8; TOKEN_LEN],
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
        })
    }

    pub fn port(&self) -> io::Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    /// Accept exactly one sender, receive everything it offers, then return.
    pub fn accept_one(&self, progress: &Progress) -> io::Result<Stats> {
        let (mut stream, peer) = self.listener.accept()?;
        sys::set_socket_buffers(stream.as_raw_fd(), sys::SOCKET_BUFFER_BYTES)?;
        self.verify_handshake(&mut stream, peer)?;
        self.receive_entries(&mut stream, progress)
    }

    fn verify_handshake(
        &self,
        stream: &mut TcpStream,
        peer: std::net::SocketAddr,
    ) -> io::Result<()> {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header)?;
        if u32::from_le_bytes(header[..4].try_into().unwrap()) != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a thunderflash sender",
            ));
        }
        if header[4] != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "sender speaks protocol v{}, this build speaks v{VERSION}",
                    header[4]
                ),
            ));
        }

        let mut token = [0u8; TOKEN_LEN];
        stream.read_exact(&mut token)?;
        if !ct_eq(&token, &self.token) {
            eprintln!("refused a connection from {peer}: token mismatch");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "token mismatch",
            ));
        }
        Ok(())
    }

    fn receive_entries(&self, stream: &mut TcpStream, progress: &Progress) -> io::Result<Stats> {
        let root = sys::open_dir(&self.dest)?;
        let mut stats = Stats::default();
        let mut buf = vec![0u8; CHUNK];

        loop {
            let Some(entry) = Entry::decode(stream)? else {
                break;
            };

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
            let dir = sys::walk_dirs(root.as_fd(), parents)?;

            progress.set_current(&entry.path);

            match entry.kind {
                Kind::Dir => {
                    sys::mkdir_at(dir.as_fd(), name, entry.mode)?;
                }
                Kind::Symlink => {
                    let mut target = vec![0u8; entry.size as usize];
                    stream.read_exact(&mut target)?;
                    let target = String::from_utf8(target).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "symlink target is not UTF-8")
                    })?;
                    // Replace any existing entry so repeat transfers work.
                    let _ = sys::unlink_at(dir.as_fd(), name);
                    sys::symlink_at(&target, dir.as_fd(), name)?;
                    // Counted so both ends agree on the byte total; the
                    // sender puts the target on the wire the same way.
                    stats.bytes += entry.size;
                }
                Kind::File => {
                    self.write_file(stream, dir.as_fd(), name, &entry, &mut buf, progress)?;
                    stats.bytes += entry.size;
                }
                Kind::End => unreachable!("decode returns None for the terminator"),
            }

            if entry.kind != Kind::Symlink {
                let _ = sys::set_mtime_at(dir.as_fd(), name, entry.mtime);
            }
            stats.files += 1;
            progress.finish_file();
        }

        // Only now is every byte committed, so it is safe to let the sender
        // claim success.
        stream.write_all(&[ACK])?;
        stream.flush()?;
        Ok(stats)
    }

    fn write_file(
        &self,
        stream: &mut TcpStream,
        dir: BorrowedFd,
        name: &str,
        entry: &Entry,
        buf: &mut [u8],
        progress: &Progress,
    ) -> io::Result<()> {
        let mut file = sys::create_file_at(dir, name, entry.mode)?;
        sys::set_nocache(file.as_raw_fd())?;

        let mut remaining = entry.size;
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            match stream.read(&mut buf[..want]) {
                Ok(0) => {
                    // Connection dropped mid-file. A partial file that looks
                    // complete is worse than no file at all.
                    drop(file);
                    let _ = sys::unlink_at(dir, name);
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
                Ok(n) => {
                    file.write_all(&buf[..n])?;
                    remaining -= n as u64;
                    progress.add_bytes(n as u64);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    drop(file);
                    let _ = sys::unlink_at(dir, name);
                    return Err(err);
                }
            }
        }
        Ok(())
    }
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

    fn handshake(token: &[u8; TOKEN_LEN]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.push(VERSION);
        buf.extend_from_slice(token);
        buf
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
            offset: 0,
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
            offset: 0,
        }
        .encode(&mut buf);
        buf.extend_from_slice(target.as_bytes());
        Entry {
            kind: Kind::File,
            path: "x/escaped.txt".into(),
            size: 5,
            mode: 0o644,
            mtime: 0,
            offset: 0,
        }
        .encode(&mut buf);
        buf.extend_from_slice(b"pwned");
        let _ = stream.write_all(&buf);
        drop(stream);

        let _ = handle.join().unwrap();
        assert!(
            !outside.join("escaped.txt").exists(),
            "receiver must not write outside the destination directory"
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
            offset: 0,
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
