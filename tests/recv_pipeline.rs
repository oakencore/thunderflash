//! Socket-level tests for the receiver's net/writer pipeline.
//!
//! These drive a raw TcpStream rather than `send::send_to`, so a session can
//! be cut off exactly where it hurts.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;

use thunderflash::progress::Progress;
use thunderflash::recv::Receiver;
use thunderflash::wire::{
    ACK, CHUNK, DIGEST_LEN, Entry, HANDSHAKE_LEN, HANDSHAKE_TIMEOUT, Kind, MAGIC, Stats, TOKEN_LEN,
    VERSION, digest,
};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tf-pipe-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Hand-built so a test can put a version or flags byte on the wire that
/// `wire::encode_handshake` would never produce.
fn handshake_raw(token: &[u8; TOKEN_LEN], version: u8, flags: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HANDSHAKE_LEN);
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.push(version);
    buf.push(flags);
    buf.extend_from_slice(token);
    buf
}

fn handshake(token: &[u8; TOKEN_LEN]) -> Vec<u8> {
    handshake_raw(token, VERSION, 0)
}

/// Start a receiver on an ephemeral port and connect to it.
fn session(
    name: &str,
    token: [u8; TOKEN_LEN],
) -> (
    PathBuf,
    TcpStream,
    std::thread::JoinHandle<std::io::Result<Stats>>,
) {
    session_at(name, token, VERSION, 0)
}

fn session_at(
    name: &str,
    token: [u8; TOKEN_LEN],
    version: u8,
    flags: u8,
) -> (
    PathBuf,
    TcpStream,
    std::thread::JoinHandle<std::io::Result<Stats>>,
) {
    let dest = scratch(name);
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress)
    });

    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream
        .write_all(&handshake_raw(&token, version, flags))
        .unwrap();
    // These tests exercise the data phase, so they declare nothing in the
    // manifest: an empty entry list means an empty bitmap, i.e. no reply bytes
    // to read back, and the receiver skips nothing.
    stream.write_all(&[Kind::End.to_u8()]).unwrap();
    stream.flush().unwrap();
    (dest, stream, handle)
}

/// The receiver closes without acking on any failure, so a sender waiting for
/// one sees EOF rather than a byte.
fn ack_byte(stream: &mut TcpStream) -> Option<u8> {
    let mut ack = [0u8; 1];
    match stream.read(&mut ack) {
        Ok(1) => Some(ack[0]),
        _ => None,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_add(seed)).collect()
}

fn file_header(path: &str, size: u64, mtime: i64) -> Vec<u8> {
    let mut buf = Vec::new();
    Entry {
        kind: Kind::File,
        path: path.to_string(),
        size,
        mode: 0o644,
        mtime,
    }
    .encode(&mut buf);
    buf
}

/// The pipeline must be exact at the chunk boundary, either side of it, and
/// at zero, since those are where a buffer-recycling bug would show up.
#[test]
fn files_around_the_chunk_boundary_arrive_byte_identical() {
    const MTIME: i64 = 1_600_000_000;
    let sizes = [0usize, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK];
    let (dest, mut stream, handle) = session("boundary", [21u8; TOKEN_LEN]);

    let payloads: Vec<Vec<u8>> = sizes
        .iter()
        .enumerate()
        .map(|(i, &size)| pattern(size, i as u8))
        .collect();

    let writer = std::thread::spawn(move || {
        for (i, payload) in payloads.iter().enumerate() {
            stream
                .write_all(&file_header(
                    &format!("f{i}.bin"),
                    payload.len() as u64,
                    MTIME,
                ))
                .unwrap();
            stream.write_all(payload).unwrap();
            stream.write_all(&digest(payload)).unwrap();
        }
        stream.write_all(&[Kind::End.to_u8()]).unwrap();
        stream.flush().unwrap();

        let mut ack = [0u8; 1];
        stream.read_exact(&mut ack).unwrap();
        assert_eq!(ack[0], ACK, "receiver must acknowledge a clean session");
    });

    let stats = handle.join().unwrap().unwrap();
    writer.join().unwrap();

    assert_eq!(stats.files, sizes.len() as u64);
    assert_eq!(stats.bytes, sizes.iter().sum::<usize>() as u64);

    for (i, &size) in sizes.iter().enumerate() {
        let path = dest.join(format!("f{i}.bin"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            pattern(size, i as u8),
            "contents differ for a {size}-byte file"
        );
        let secs = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, MTIME as u64, "mtime lost on a {size}-byte file");
    }
}

/// The writer, not the network thread, owns the cleanup — so a hang-up after
/// several chunks have already been queued must still leave no file.
#[test]
fn a_hang_up_mid_multi_chunk_file_leaves_nothing_behind() {
    let (dest, mut stream, handle) = session("hangup", [22u8; TOKEN_LEN]);

    stream
        .write_all(&file_header("big.bin", 8 * CHUNK as u64, 0))
        .unwrap();
    stream.write_all(&pattern(CHUNK + 4096, 0)).unwrap();
    stream.flush().unwrap();
    drop(stream); // hang up with 7 chunks still owed

    let err = handle.join().unwrap().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "got {err}");
    assert!(
        err.to_string().starts_with("big.bin truncated: expected"),
        "unexpected message: {err}"
    );
    assert!(
        !dest.join("big.bin").exists(),
        "the partial multi-chunk file must be removed"
    );
}

/// A second session over the same destination overwrites cleanly, including
/// the mtime the writer stamps through the descriptor.
#[test]
fn a_repeat_transfer_overwrites_the_previous_contents() {
    let dest = scratch("repeat");
    for (round, seed) in [(0u8, 1u8), (1, 2)] {
        let token = [23u8; TOKEN_LEN];
        let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
        let port = receiver.port().unwrap();
        let handle = std::thread::spawn(move || {
            let progress = Progress::new("receiving");
            receiver.accept_one(&progress)
        });

        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.write_all(&handshake(&token)).unwrap();
        stream.write_all(&[Kind::End.to_u8()]).unwrap(); // empty manifest
        stream.flush().unwrap();

        let payload = pattern(CHUNK + 17, seed);
        let writer = std::thread::spawn(move || {
            stream
                .write_all(&file_header(
                    "again.bin",
                    payload.len() as u64,
                    1_700_000_000,
                ))
                .unwrap();
            stream.write_all(&payload).unwrap();
            stream.write_all(&digest(&payload)).unwrap();
            stream.write_all(&[Kind::End.to_u8()]).unwrap();
            stream.flush().unwrap();
            let mut ack = [0u8; 1];
            stream.read_exact(&mut ack).unwrap();
            assert_eq!(ack[0], ACK);
        });

        handle
            .join()
            .unwrap()
            .unwrap_or_else(|err| panic!("round {round} failed: {err}"));
        writer.join().unwrap();

        assert_eq!(
            std::fs::read(dest.join("again.bin")).unwrap(),
            pattern(CHUNK + 17, seed)
        );
    }
}

/// The digest is checked before the file is finished, so a corrupted payload
/// must leave nothing behind and must never be acknowledged.
#[test]
fn a_wrong_digest_deletes_the_file_and_withholds_the_ack() {
    let (dest, mut stream, handle) = session("bad-digest", [24u8; TOKEN_LEN]);
    let payload = pattern(CHUNK + 5, 3);

    stream
        .write_all(&file_header("wrong.bin", payload.len() as u64, 0))
        .unwrap();
    stream.write_all(&payload).unwrap();
    stream.write_all(&[0xABu8; DIGEST_LEN]).unwrap();
    stream.write_all(&[Kind::End.to_u8()]).unwrap();
    stream.flush().unwrap();

    let err = handle.join().unwrap().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "got {err}");
    assert!(
        err.to_string().contains("wrong.bin"),
        "the error must name the path: {err}"
    );
    assert!(
        !dest.join("wrong.bin").exists(),
        "a file that failed verification must be deleted"
    );
    assert_eq!(ack_byte(&mut stream), None, "a failed session must not ack");
}

/// The stream stays self-consistent in length — only the bytes differ — which
/// is the shape real corruption takes.
#[test]
fn a_flipped_payload_byte_fails_verification() {
    let (dest, mut stream, handle) = session("flipped", [25u8; TOKEN_LEN]);
    let payload = pattern(CHUNK + 1024, 4);
    let mut corrupted = payload.clone();
    corrupted[CHUNK / 2] ^= 0x01;

    stream
        .write_all(&file_header("flip.bin", payload.len() as u64, 0))
        .unwrap();
    stream.write_all(&corrupted).unwrap();
    // The digest of what the sender *meant* to send.
    stream.write_all(&digest(&payload)).unwrap();
    stream.write_all(&[Kind::End.to_u8()]).unwrap();
    stream.flush().unwrap();

    let err = handle.join().unwrap().unwrap_err();
    assert!(
        err.to_string().contains("flip.bin"),
        "the error must name the path: {err}"
    );
    assert!(!dest.join("flip.bin").exists(), "the file must be deleted");
    assert_eq!(ack_byte(&mut stream), None);
}

/// A connection that dies inside the digest must fail, not block forever.
#[test]
fn a_stream_that_ends_mid_digest_fails_cleanly() {
    let (dest, mut stream, handle) = session("half-digest", [26u8; TOKEN_LEN]);
    let payload = pattern(64, 5);

    stream
        .write_all(&file_header("cut.bin", payload.len() as u64, 0))
        .unwrap();
    stream.write_all(&payload).unwrap();
    stream
        .write_all(&digest(&payload)[..DIGEST_LEN - 1])
        .unwrap();
    stream.flush().unwrap();
    drop(stream);

    let err = handle.join().unwrap().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "got {err}");
    assert!(
        !dest.join("cut.bin").exists(),
        "a file whose digest never arrived must be deleted"
    );
}

/// Zero-length files still carry the digest of empty input.
#[test]
fn an_empty_file_is_verified_against_its_digest() {
    let (dest, mut stream, handle) = session("empty-digest", [27u8; TOKEN_LEN]);

    stream.write_all(&file_header("zero.bin", 0, 0)).unwrap();
    stream.write_all(&digest(b"")).unwrap();
    stream.write_all(&[Kind::End.to_u8()]).unwrap();
    stream.flush().unwrap();
    assert_eq!(ack_byte(&mut stream), Some(ACK));

    handle.join().unwrap().unwrap();
    assert_eq!(std::fs::read(dest.join("zero.bin")).unwrap().len(), 0);
}

#[test]
fn an_empty_file_with_a_wrong_digest_is_refused() {
    let (dest, mut stream, handle) = session("empty-bad", [28u8; TOKEN_LEN]);

    stream.write_all(&file_header("zero.bin", 0, 0)).unwrap();
    stream.write_all(&[0u8; DIGEST_LEN]).unwrap();
    stream.write_all(&[Kind::End.to_u8()]).unwrap();
    stream.flush().unwrap();

    assert!(handle.join().unwrap().is_err());
    assert!(!dest.join("zero.bin").exists());
    assert_eq!(ack_byte(&mut stream), None);
}

/// Symlink targets are verified too, and a bad one must not be created.
#[test]
fn a_symlink_with_a_wrong_digest_is_refused() {
    let (dest, mut stream, handle) = session("link-digest", [29u8; TOKEN_LEN]);
    let target = b"big/large.bin";

    let mut buf = Vec::new();
    Entry {
        kind: Kind::Symlink,
        path: "link".into(),
        size: target.len() as u64,
        mode: 0o777,
        mtime: 0,
    }
    .encode(&mut buf);
    buf.extend_from_slice(target);
    buf.extend_from_slice(&[0x5Au8; DIGEST_LEN]);
    buf.push(Kind::End.to_u8());
    stream.write_all(&buf).unwrap();
    stream.flush().unwrap();

    let err = handle.join().unwrap().unwrap_err();
    assert!(err.to_string().contains("link"), "got {err}");
    assert!(
        std::fs::symlink_metadata(dest.join("link")).is_err(),
        "an unverified symlink must not be created"
    );
    assert_eq!(ack_byte(&mut stream), None);
}

#[test]
fn a_symlink_with_a_matching_digest_is_created() {
    let (dest, mut stream, handle) = session("link-ok", [30u8; TOKEN_LEN]);
    let target = b"big/large.bin";

    let mut buf = Vec::new();
    Entry {
        kind: Kind::Symlink,
        path: "link".into(),
        size: target.len() as u64,
        mode: 0o777,
        mtime: 0,
    }
    .encode(&mut buf);
    buf.extend_from_slice(target);
    buf.extend_from_slice(&digest(target));
    buf.push(Kind::End.to_u8());
    stream.write_all(&buf).unwrap();
    stream.flush().unwrap();
    assert_eq!(ack_byte(&mut stream), Some(ACK));

    handle.join().unwrap().unwrap();
    assert_eq!(
        std::fs::read_link(dest.join("link")).unwrap().to_str(),
        Some("big/large.bin")
    );
}

/// A flag bit this build does not know means framing it cannot parse, so the
/// session must die at the handshake rather than misread the stream.
#[test]
fn an_unknown_handshake_flag_is_refused() {
    let (dest, mut stream, handle) = session_at("bad-flag", [32u8; TOKEN_LEN], VERSION, 0x80);

    let payload = b"abc";
    stream
        .write_all(&file_header("nope.bin", payload.len() as u64, 0))
        .unwrap();
    let _ = stream.write_all(payload);
    stream.flush().unwrap();

    let err = handle.join().unwrap().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("unknown handshake flags"),
        "got {err}"
    );
    assert!(!dest.join("nope.bin").exists());
    assert_eq!(ack_byte(&mut stream), None);
}

/// With FLAG_NO_VERIFY the stream carries no digests at all: a payload
/// followed straight by the next header must be accepted and acked.
#[test]
fn a_no_verify_stream_carries_no_digests() {
    let (dest, mut stream, handle) = session_at(
        "no-verify",
        [33u8; TOKEN_LEN],
        VERSION,
        thunderflash::wire::FLAG_NO_VERIFY,
    );
    let payload = pattern(CHUNK + 9, 6);

    stream
        .write_all(&file_header("plain.bin", payload.len() as u64, 0))
        .unwrap();
    stream.write_all(&payload).unwrap();
    stream.write_all(&file_header("zero.bin", 0, 0)).unwrap();
    stream.write_all(&[Kind::End.to_u8()]).unwrap();
    stream.flush().unwrap();
    assert_eq!(ack_byte(&mut stream), Some(ACK));

    let stats = handle.join().unwrap().unwrap();
    assert_eq!(stats.files, 2);
    assert_eq!(std::fs::read(dest.join("plain.bin")).unwrap(), payload);
    assert_eq!(std::fs::read(dest.join("zero.bin")).unwrap().len(), 0);
}

/// An older sender and this receiver refuse each other rather than
/// misreading the other's framing.
#[test]
fn a_v1_sender_is_refused_by_a_v3_receiver() {
    let (dest, mut stream, handle) = session_at("v1-sender", [31u8; TOKEN_LEN], 1, 0);

    let payload = b"abc";
    stream
        .write_all(&file_header("old.bin", payload.len() as u64, 0))
        .unwrap();
    let _ = stream.write_all(payload);
    stream.flush().unwrap();

    let err = handle.join().unwrap().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("v1") && err.to_string().contains("v3"),
        "the version error must name both versions: {err}"
    );
    assert!(!dest.join("old.bin").exists());
    assert_eq!(ack_byte(&mut stream), None);
}

/// The receiver keeps the previous entry's parent directory open, so a stream
/// that hops between directories must still land every file in the right one.
#[test]
fn entries_alternating_between_parents_land_in_the_right_directories() {
    let (dest, mut stream, handle) = session("alternating", [44u8; TOKEN_LEN]);
    let files = [
        ("a/f1.bin", 1u8),
        ("b/f2.bin", 2u8),
        ("a/f3.bin", 3u8),
        ("b/f4.bin", 4u8),
        ("c/deep/f5.bin", 5u8),
        ("a/f6.bin", 6u8),
    ];

    for (path, seed) in files {
        let payload = pattern(64, seed);
        stream
            .write_all(&file_header(path, payload.len() as u64, 0))
            .unwrap();
        stream.write_all(&payload).unwrap();
        stream.write_all(&digest(&payload)).unwrap();
    }
    stream.write_all(&[Kind::End.to_u8()]).unwrap();
    stream.flush().unwrap();
    assert_eq!(ack_byte(&mut stream), Some(ACK));

    let stats = handle.join().unwrap().unwrap();
    assert_eq!(stats.files, files.len() as u64);
    for (path, seed) in files {
        assert_eq!(
            std::fs::read(dest.join(path)).unwrap(),
            pattern(64, seed),
            "{path} did not arrive in its own directory"
        );
    }
}

/// The cached parent must never stand in for a *different* directory: a name
/// that turns into a symlink while another directory is cached still has to
/// meet walk_dirs' O_NOFOLLOW walk, and be refused.
#[test]
fn a_parent_that_became_a_symlink_is_still_refused_while_another_is_cached() {
    let outside = scratch("cache-escape-outside");
    let (dest, mut stream, handle) = session("cache-escape", [45u8; TOKEN_LEN]);

    // First entry caches parent "a".
    let payload = pattern(32, 9);
    stream
        .write_all(&file_header("a/f1.bin", payload.len() as u64, 0))
        .unwrap();
    stream.write_all(&payload).unwrap();
    stream.write_all(&digest(&payload)).unwrap();
    stream.flush().unwrap();

    // The file exists only once the net thread has walked "a", so this is the
    // point where the cache definitely holds it.
    let landed = dest.join("a/f1.bin");
    for _ in 0..400 {
        if landed.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(landed.exists(), "first entry never landed");

    // Now "b" appears as a symlink pointing out of the destination.
    std::os::unix::fs::symlink(&outside, dest.join("b")).unwrap();

    let loot = pattern(16, 7);
    stream
        .write_all(&file_header("b/loot.bin", loot.len() as u64, 0))
        .unwrap();
    let _ = stream.write_all(&loot);
    let _ = stream.flush();

    assert!(
        handle.join().unwrap().is_err(),
        "a symlinked parent must fail the transfer"
    );
    assert!(
        !outside.join("loot.bin").exists(),
        "nothing may be written outside the destination"
    );
    assert_eq!(ack_byte(&mut stream), None);
}

/// A peer that connects and then says nothing must not pin the receiver: this
/// process accepts exactly one connection, so an unbounded read here is the
/// whole session, and any local process can open it.
#[test]
fn a_silent_peer_times_out_instead_of_pinning_the_receiver() {
    let dest = scratch("silent-peer");
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, [61u8; TOKEN_LEN]).unwrap();
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress)
    });

    // Connected and held open, with not one byte of handshake written.
    let _silent = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();

    let started = std::time::Instant::now();
    let err = handle.join().unwrap().unwrap_err();
    let elapsed = started.elapsed();

    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "expected a timeout, got {err:?}"
    );
    assert!(
        elapsed < HANDSHAKE_TIMEOUT * 4,
        "receiver took {elapsed:?} to give up"
    );
}
