#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Absolute,
    ParentRef,
    Nul,
    TooLong,
}

use std::io::{self, Read, Write};

/// "TFLS" — identifies our protocol on the first four bytes of a connection.
pub const MAGIC: u32 = 0x5446_4C53;
pub const VERSION: u8 = 1;
pub const TOKEN_LEN: usize = 32;
/// Sent by the receiver once every entry is written, so the sender never
/// reports success for data the receiver has not actually committed.
pub const ACK: u8 = 0xFF;
/// Read/write chunk size. Large enough that syscall overhead disappears
/// against a multi-gigabit link. Shared so both ends agree.
pub const CHUNK: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    End,
    File,
    Dir,
    Symlink,
}

impl Kind {
    pub fn to_u8(self) -> u8 {
        match self {
            Kind::End => 0,
            Kind::File => 1,
            Kind::Dir => 2,
            Kind::Symlink => 3,
        }
    }

    pub fn from_u8(byte: u8) -> Option<Kind> {
        match byte {
            0 => Some(Kind::End),
            1 => Some(Kind::File),
            2 => Some(Kind::Dir),
            3 => Some(Kind::Symlink),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub path: String,
    /// File: byte length. Symlink: length of the target string. Dir: 0.
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
    /// Always 0 in v1. Reserved so a future parallel-chunked sender is a
    /// small delta rather than a wire format break.
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub files: u64,
    pub bytes: u64,
}

impl Entry {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.kind.to_u8());
        out.extend_from_slice(&(self.path.len() as u16).to_le_bytes());
        out.extend_from_slice(self.path.as_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.mode.to_le_bytes());
        out.extend_from_slice(&self.mtime.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
    }

    /// Read one entry. Returns `Ok(None)` when the terminator is reached.
    pub fn decode(reader: &mut impl Read) -> io::Result<Option<Entry>> {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        let kind = Kind::from_u8(byte[0]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown entry kind {}", byte[0]),
            )
        })?;
        if kind == Kind::End {
            return Ok(None);
        }

        let mut len_bytes = [0u8; 2];
        reader.read_exact(&mut len_bytes)?;
        let mut path_bytes = vec![0u8; u16::from_le_bytes(len_bytes) as usize];
        reader.read_exact(&mut path_bytes)?;
        let path = String::from_utf8(path_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "path is not valid UTF-8"))?;

        let mut u64_buf = [0u8; 8];
        let mut u32_buf = [0u8; 4];

        reader.read_exact(&mut u64_buf)?;
        let size = u64::from_le_bytes(u64_buf);
        reader.read_exact(&mut u32_buf)?;
        let mode = u32::from_le_bytes(u32_buf);
        reader.read_exact(&mut u64_buf)?;
        let mtime = i64::from_le_bytes(u64_buf);
        reader.read_exact(&mut u64_buf)?;
        let offset = u64::from_le_bytes(u64_buf);

        Ok(Some(Entry {
            kind,
            path,
            size,
            mode,
            mtime,
            offset,
        }))
    }
}

pub fn write_terminator(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(&[Kind::End.to_u8()])
}

/// Compare two byte slices without leaking their contents through timing.
/// Used for the shared token, where an early-exit compare would let a peer
/// recover the token byte by byte.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Maximum accepted path length in bytes. Comfortably above any real path,
/// low enough that a hostile sender cannot make us allocate wildly.
const MAX_PATH_LEN: usize = 4096;

/// Split a sender-supplied relative path into components, rejecting anything
/// that could escape the destination directory at the string level.
///
/// Passing this check is necessary but NOT sufficient — see `sys::walk_dirs`,
/// which prevents escape via symlinks that these string rules cannot see.
pub fn sanitize(path: &str) -> Result<Vec<&str>, PathError> {
    if path.len() > MAX_PATH_LEN {
        return Err(PathError::TooLong);
    }
    if path.contains('\0') {
        return Err(PathError::Nul);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute);
    }

    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(PathError::ParentRef),
            other => parts.push(other),
        }
    }

    if parts.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_normal_nested_path() {
        assert_eq!(
            sanitize("photos/2019/img.heic"),
            Ok(vec!["photos", "2019", "img.heic"])
        );
    }

    #[test]
    fn accepts_a_bare_filename() {
        assert_eq!(sanitize("archive.dmg"), Ok(vec!["archive.dmg"]));
    }

    #[test]
    fn drops_dot_and_empty_segments() {
        assert_eq!(sanitize("a/./b"), Ok(vec!["a", "b"]));
        assert_eq!(sanitize("a//b"), Ok(vec!["a", "b"]));
    }

    #[test]
    fn rejects_absolute_paths() {
        assert_eq!(sanitize("/etc/passwd"), Err(PathError::Absolute));
    }

    #[test]
    fn rejects_parent_references_anywhere() {
        assert_eq!(sanitize("../../etc/passwd"), Err(PathError::ParentRef));
        assert_eq!(sanitize("a/../../b"), Err(PathError::ParentRef));
        assert_eq!(sanitize("a/b/.."), Err(PathError::ParentRef));
    }

    #[test]
    fn rejects_embedded_nul() {
        assert_eq!(sanitize("a\0b"), Err(PathError::Nul));
    }

    #[test]
    fn rejects_paths_that_resolve_to_nothing() {
        assert_eq!(sanitize(""), Err(PathError::Empty));
        assert_eq!(sanitize("."), Err(PathError::Empty));
        assert_eq!(sanitize("./"), Err(PathError::Empty));
    }

    #[test]
    fn rejects_overlong_paths() {
        let long = "a/".repeat(3000);
        assert_eq!(sanitize(&long), Err(PathError::TooLong));
    }

    fn roundtrip(entry: &Entry) -> Entry {
        let mut buf = Vec::new();
        entry.encode(&mut buf);
        let mut cursor = std::io::Cursor::new(buf);
        Entry::decode(&mut cursor).unwrap().unwrap()
    }

    #[test]
    fn roundtrips_a_file_entry() {
        let entry = Entry {
            kind: Kind::File,
            path: "photos/2019/img.heic".to_string(),
            size: 4_294_967_296,
            mode: 0o644,
            mtime: 1_755_400_000,
            offset: 0,
        };
        assert_eq!(roundtrip(&entry), entry);
    }

    #[test]
    fn roundtrips_a_zero_length_file() {
        let entry = Entry {
            kind: Kind::File,
            path: "empty.txt".to_string(),
            size: 0,
            mode: 0o600,
            mtime: 0,
            offset: 0,
        };
        assert_eq!(roundtrip(&entry), entry);
    }

    #[test]
    fn roundtrips_a_directory_entry() {
        let entry = Entry {
            kind: Kind::Dir,
            path: "empty-dir".to_string(),
            size: 0,
            mode: 0o755,
            mtime: 1_755_400_000,
            offset: 0,
        };
        assert_eq!(roundtrip(&entry), entry);
    }

    #[test]
    fn roundtrips_a_symlink_entry() {
        let entry = Entry {
            kind: Kind::Symlink,
            path: "node_modules/.bin/tsc".to_string(),
            size: 24,
            mode: 0o777,
            mtime: 1_755_400_000,
            offset: 0,
        };
        assert_eq!(roundtrip(&entry), entry);
    }

    #[test]
    fn roundtrips_a_negative_mtime() {
        let entry = Entry {
            kind: Kind::File,
            path: "old".to_string(),
            size: 1,
            mode: 0o644,
            mtime: -86_400,
            offset: 0,
        };
        assert_eq!(roundtrip(&entry), entry);
    }

    #[test]
    fn decode_returns_none_at_terminator() {
        let mut buf = Vec::new();
        write_terminator(&mut buf).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(Entry::decode(&mut cursor).unwrap(), None);
    }

    #[test]
    fn decode_rejects_an_unknown_kind_byte() {
        let mut cursor = std::io::Cursor::new(vec![99u8]);
        assert!(Entry::decode(&mut cursor).is_err());
    }

    #[test]
    fn decode_rejects_non_utf8_paths() {
        let mut buf = vec![Kind::File.to_u8()];
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&[0xff, 0xfe]);
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0i64.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        assert!(Entry::decode(&mut cursor).is_err());
    }

    #[test]
    fn ct_eq_matches_only_identical_slices() {
        assert!(ct_eq(&[1, 2, 3], &[1, 2, 3]));
        assert!(!ct_eq(&[1, 2, 3], &[1, 2, 4]));
        assert!(!ct_eq(&[1, 2, 3], &[1, 2]));
        assert!(ct_eq(&[], &[]));
    }
}
