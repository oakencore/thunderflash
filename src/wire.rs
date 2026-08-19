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
/// v3 adds a manifest phase: the entry list travels first, the receiver
/// answers with a bitmap of the files it already has, and the data phase
/// omits those entirely.
pub const VERSION: u8 = 3;
pub const TOKEN_LEN: usize = 32;
/// MAGIC(4) VERSION(1) FLAGS(1) TOKEN(32).
pub const HANDSHAKE_LEN: usize = 6 + TOKEN_LEN;
/// Sender opted out of verification: the stream carries NO digest frames.
pub const FLAG_NO_VERIFY: u8 = 0x01;
/// Every bit this build understands. A sender setting anything else is
/// speaking a framing we cannot parse, so the connection is refused.
const KNOWN_FLAGS: u8 = FLAG_NO_VERIFY;
/// Sent by the receiver once every entry is written, so the sender never
/// reports success for data the receiver has not actually committed.
pub const ACK: u8 = 0xFF;
/// Read/write chunk size. Large enough that syscall overhead disappears
/// against a multi-gigabit link. Shared so both ends agree.
pub const CHUNK: usize = 4 * 1024 * 1024;
/// Length of the BLAKE3 digest that follows every File and Symlink payload.
pub const DIGEST_LEN: usize = 32;
/// Buffers at or above this size are hashed with `Hasher::update_rayon`,
/// which parallelises chunk compression across cores; below it the join
/// overhead loses to the work, so it stays on plain `update`.
pub const RAYON_MIN: usize = 256 * 1024;
pub type Digest = [u8; DIGEST_LEN];

/// A peer that has said nothing for this long is gone, not slow: both ends
/// are on one cable and neither pauses mid-stream. Without it a silent peer
/// pins the one-shot receiver (or the sender's ACK wait) forever, since
/// nothing else ever times a TCP read out.
pub const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// The handshake is 38 bytes written immediately after connect, so a real
/// sender never needs longer.
pub const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bound how long either end will block on a peer that has gone quiet.
pub fn set_timeouts(stream: &std::net::TcpStream, timeout: std::time::Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}

/// Chunk buffers in flight per side. Benchmark knob only — deliberately not a
/// CLI flag, since the default is right for every link this tool targets.
/// The receiver holds exactly this many CHUNK buffers; the sender holds two
/// more (one being filled, one being written).
const QUEUE_DEPTH: usize = 4;

fn clamp_depth(raw: Option<&str>) -> usize {
    match raw.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) => n.clamp(1, 64),
        None => QUEUE_DEPTH,
    }
}

pub fn queue_depth() -> usize {
    static DEPTH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DEPTH.get_or_init(|| clamp_depth(std::env::var("TF_QUEUE_DEPTH").ok().as_deref()))
}

/// One-shot digest of a whole payload. The transfer paths hash incrementally
/// as bytes pass through; this is for short payloads and for tests.
pub fn digest(bytes: &[u8]) -> Digest {
    *blake3::hash(bytes).as_bytes()
}

/// Incremental hash of one pooled buffer, on whichever BLAKE3 path suits its
/// size: `update_rayon` pays a join overhead that small buffers do not earn
/// back, so they stay on the plain path.
pub fn hash_update(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    if bytes.len() >= RAYON_MIN {
        hasher.update_rayon(bytes);
    } else {
        hasher.update(bytes);
    }
}

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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub files: u64,
    pub bytes: u64,
    /// Files the receiver already had, so nothing moved for them. Counted on
    /// both sides from the same bitmap.
    pub skipped_files: u64,
    pub skipped_bytes: u64,
}

impl Entry {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.kind.to_u8());
        out.extend_from_slice(&(self.path.len() as u16).to_le_bytes());
        out.extend_from_slice(self.path.as_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.mode.to_le_bytes());
        out.extend_from_slice(&self.mtime.to_le_bytes());
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
        // A symlink target is a path, so it can never legitimately exceed
        // PATH_MAX. Refused here rather than at the receiver's symlink arm,
        // since `size` is the length it would allocate on the sender's say-so.
        if kind == Kind::Symlink && size > MAX_PATH_LEN as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("symlink target of {size} bytes exceeds {MAX_PATH_LEN}"),
            ));
        }
        reader.read_exact(&mut u32_buf)?;
        let mode = u32::from_le_bytes(u32_buf);
        reader.read_exact(&mut u64_buf)?;
        let mtime = i64::from_le_bytes(u64_buf);

        Ok(Some(Entry {
            kind,
            path,
            size,
            mode,
            mtime,
        }))
    }
}

pub fn encode_handshake(flags: u8, token: &[u8; TOKEN_LEN]) -> [u8; HANDSHAKE_LEN] {
    let mut buf = [0u8; HANDSHAKE_LEN];
    buf[..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4] = VERSION;
    buf[5] = flags;
    buf[6..].copy_from_slice(token);
    buf
}

/// Parse a handshake into its flags and token. Rejects a foreign protocol, a
/// version we do not speak, and any flag bit we do not understand — all before
/// the token is even compared, since none of them can be honoured.
pub fn decode_handshake(buf: &[u8; HANDSHAKE_LEN]) -> io::Result<(u8, [u8; TOKEN_LEN])> {
    if u32::from_le_bytes(buf[..4].try_into().unwrap()) != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a thunderflash sender",
        ));
    }
    if buf[4] != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "sender speaks protocol v{}, this build speaks v{VERSION}",
                buf[4]
            ),
        ));
    }
    let flags = buf[5];
    if flags & !KNOWN_FLAGS != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sender set unknown handshake flags 0x{flags:02x}"),
        ));
    }
    Ok((flags, buf[6..].try_into().unwrap()))
}

pub fn write_terminator(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(&[Kind::End.to_u8()])
}

/// Pack one bit per File entry, in wire order, LSB-first within each byte.
///
/// The length is implied: both ends counted the same File entries out of the
/// manifest, so only the bits travel.
pub fn encode_bitmap(bits: &[bool]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, &bit) in bits.iter().enumerate() {
        if bit {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

/// Read the bitmap answering a manifest of `n` File entries.
pub fn read_bitmap(reader: &mut impl Read, n: usize) -> io::Result<Vec<bool>> {
    let mut bytes = vec![0u8; n.div_ceil(8)];
    reader.read_exact(&mut bytes)?;
    Ok((0..n).map(|i| bytes[i / 8] >> (i % 8) & 1 == 1).collect())
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
pub const MAX_PATH_LEN: usize = 4096;

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
        let mut cursor = std::io::Cursor::new(buf);
        assert!(Entry::decode(&mut cursor).is_err());
    }

    /// A hostile sender announcing a huge symlink target must be refused at
    /// decode, before anyone allocates on its say-so.
    #[test]
    fn decode_rejects_a_symlink_target_larger_than_a_path() {
        let mut buf = Vec::new();
        Entry {
            kind: Kind::Symlink,
            path: "x".into(),
            size: u64::MAX,
            mode: 0o777,
            mtime: 0,
        }
        .encode(&mut buf);
        let err = Entry::decode(&mut std::io::Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds"), "{err}");

        // A target right at the bound is still legal.
        let mut buf = Vec::new();
        Entry {
            kind: Kind::Symlink,
            path: "x".into(),
            size: MAX_PATH_LEN as u64,
            mode: 0o777,
            mtime: 0,
        }
        .encode(&mut buf);
        assert!(Entry::decode(&mut std::io::Cursor::new(buf)).is_ok());
    }

    /// A File entry keeps its full u64 size: files really are that big.
    #[test]
    fn decode_still_accepts_a_huge_file_size() {
        let mut buf = Vec::new();
        Entry {
            kind: Kind::File,
            path: "big".into(),
            size: 1 << 45,
            mode: 0o644,
            mtime: 0,
        }
        .encode(&mut buf);
        assert!(Entry::decode(&mut std::io::Cursor::new(buf)).is_ok());
    }

    #[test]
    fn queue_depth_env_override_is_clamped() {
        assert_eq!(clamp_depth(None), QUEUE_DEPTH);
        assert_eq!(clamp_depth(Some("garbage")), QUEUE_DEPTH);
        assert_eq!(clamp_depth(Some("0")), 1);
        assert_eq!(clamp_depth(Some(" 16 ")), 16);
        assert_eq!(clamp_depth(Some("9999")), 64);
    }

    /// The empty-input digest is a fixed BLAKE3 constant, so a zero-length
    /// file still has something meaningful to verify.
    #[test]
    fn digest_of_empty_input_is_the_known_blake3_constant() {
        let expected =
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262".to_string();
        let hex: String = digest(b"").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected);
    }

    #[test]
    fn digest_changes_when_a_single_byte_changes() {
        let mut data = vec![7u8; CHUNK + 1];
        let before = digest(&data);
        data[CHUNK / 2] ^= 1;
        assert_ne!(digest(&data), before);
        assert_eq!(before.len(), DIGEST_LEN);
    }

    /// The receiver hashes chunk by chunk; that must equal the one-shot digest
    /// the `digest` helper (and the tests) produce.
    #[test]
    fn incremental_hashing_matches_the_one_shot_digest() {
        let data: Vec<u8> = (0..CHUNK * 2 + 9).map(|i| (i % 251) as u8).collect();
        let mut hasher = blake3::Hasher::new();
        for chunk in data.chunks(CHUNK) {
            hasher.update(chunk);
        }
        assert_eq!(*hasher.finalize().as_bytes(), digest(&data));
    }

    #[test]
    fn handshake_round_trips_flags_and_token() {
        let token = [0xA5u8; TOKEN_LEN];
        for flags in [0, FLAG_NO_VERIFY] {
            let buf = encode_handshake(flags, &token);
            assert_eq!(decode_handshake(&buf).unwrap(), (flags, token));
        }
    }

    #[test]
    fn handshake_rejects_an_unknown_flag_bit() {
        let mut buf = encode_handshake(FLAG_NO_VERIFY, &[1u8; TOKEN_LEN]);
        buf[5] |= 0x80;
        let err = decode_handshake(&buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("unknown handshake flags"), "{err}");
    }

    #[test]
    fn handshake_rejects_a_foreign_magic_and_a_wrong_version() {
        let mut buf = encode_handshake(0, &[1u8; TOKEN_LEN]);
        buf[0] ^= 0xFF;
        assert!(decode_handshake(&buf).is_err());

        let mut buf = encode_handshake(0, &[1u8; TOKEN_LEN]);
        buf[4] = 1;
        let err = decode_handshake(&buf).unwrap_err();
        assert!(err.to_string().contains("v1"), "{err}");
    }

    fn bitmap_roundtrip(bits: &[bool]) -> Vec<bool> {
        let bytes = encode_bitmap(bits);
        assert_eq!(bytes.len(), bits.len().div_ceil(8));
        read_bitmap(&mut std::io::Cursor::new(bytes), bits.len()).unwrap()
    }

    #[test]
    fn bitmap_round_trips_a_whole_number_of_bytes() {
        let bits: Vec<bool> = (0..16).map(|i| i % 3 == 0).collect();
        assert_eq!(bitmap_roundtrip(&bits), bits);
    }

    #[test]
    fn bitmap_round_trips_a_length_that_is_not_a_multiple_of_eight() {
        for n in [1, 7, 9, 15, 100] {
            let bits: Vec<bool> = (0..n).map(|i| i % 5 < 2).collect();
            assert_eq!(bitmap_roundtrip(&bits), bits, "n = {n}");
        }
    }

    #[test]
    fn bitmap_of_no_files_is_zero_bytes() {
        assert!(encode_bitmap(&[]).is_empty());
        assert_eq!(bitmap_roundtrip(&[]), Vec::<bool>::new());
    }

    /// LSB-first within each byte: entry 0 is bit 0 of byte 0.
    #[test]
    fn bitmap_packs_the_first_entry_into_the_lowest_bit() {
        assert_eq!(
            encode_bitmap(&[true, false, false, false]),
            vec![0b0000_0001]
        );
        assert_eq!(encode_bitmap(&[false; 8]), vec![0u8]);
        assert_eq!(encode_bitmap(&[true; 9]), vec![0xFF, 0b0000_0001]);
    }

    /// The padding bits of a short final byte carry no entries, so whatever a
    /// peer leaves in them must not become extra answers.
    #[test]
    fn bitmap_ignores_padding_bits_in_the_final_byte() {
        let bits = read_bitmap(&mut std::io::Cursor::new(vec![0xFFu8]), 3).unwrap();
        assert_eq!(bits, vec![true, true, true]);
    }

    #[test]
    fn read_bitmap_fails_on_a_truncated_reply() {
        let err = read_bitmap(&mut std::io::Cursor::new(vec![0u8]), 9).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn ct_eq_matches_only_identical_slices() {
        assert!(ct_eq(&[1, 2, 3], &[1, 2, 3]));
        assert!(!ct_eq(&[1, 2, 3], &[1, 2, 4]));
        assert!(!ct_eq(&[1, 2, 3], &[1, 2]));
        assert!(ct_eq(&[], &[]));
    }
}
