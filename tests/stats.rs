//! `--stats` counters must agree with the Stats both ends already report.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use thunderflash::progress::{Diag, Progress};
use thunderflash::recv::Receiver;
use thunderflash::send;
use thunderflash::wire::{CHUNK, DIGEST_LEN, TOKEN_LEN};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tf-stats-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Two dirs, two files (one spanning three chunks), one symlink — every
/// counter has a known value.
const BIG: usize = 2 * CHUNK + 5;
const SMALL: &[u8] = b"hello";
const LINK: &str = "sub/a.txt";
const CHUNKS: u64 = 4; // three for BIG, one for SMALL
const BUFFERS: u64 = 6; // default queue depth of 4, plus the sender's two spares

#[test]
fn diagnostics_match_the_transfer_on_both_ends() {
    let source = scratch("source");
    let dest = scratch("dest");
    std::fs::create_dir_all(source.join("tree/sub")).unwrap();
    std::fs::write(
        source.join("tree/big.bin"),
        (0..BIG).map(|i| i as u8).collect::<Vec<u8>>(),
    )
    .unwrap();
    std::fs::write(source.join("tree/sub/a.txt"), SMALL).unwrap();
    std::os::unix::fs::symlink(LINK, source.join("tree/link")).unwrap();

    let payload = (BIG + SMALL.len() + LINK.len()) as u64;
    let on_disk = (BIG + SMALL.len()) as u64;

    let token = [77u8; TOKEN_LEN];
    let recv_diag = Arc::new(Diag::default());
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token)
        .unwrap()
        .with_diag(Arc::clone(&recv_diag));
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });

    let send_diag = Arc::new(Diag::default());
    let progress = Progress::new("sending");
    let sent = send::send_to_diag(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &[source.join("tree")],
        &token,
        &progress,
        true,
        Some(Arc::clone(&send_diag)),
    )
    .unwrap();
    let received = handle.join().unwrap();

    for (side, diag) in [("send", &send_diag), ("recv", &recv_diag)] {
        let (files, dirs, links) = (
            diag.files.load(Relaxed),
            diag.dirs.load(Relaxed),
            diag.symlinks.load(Relaxed),
        );
        assert_eq!((files, dirs, links), (2, 2, 1), "{side} entry counts");
        // They must also add up to the entry count Stats reports.
        assert_eq!(files + dirs + links, sent.files, "{side} entries vs Stats");
        // Nothing can be in flight without a pooled buffer holding it, and the
        // sender owns the larger pool (queue depth plus two).
        let peak = diag.queue_peak.load(Relaxed);
        assert!((1..=BUFFERS).contains(&peak), "{side} queue peak {peak}");
    }

    // Stall counts are timing-dependent, but the sender takes exactly one
    // buffer per chunk, so it can never stall more often than that.
    assert!(
        send_diag.pool_stalls.load(Relaxed) <= CHUNKS,
        "sender stalled more often than it took a buffer"
    );

    assert_eq!(sent.bytes, payload);
    assert_eq!(received.bytes, payload);

    // Sender: every payload byte came off the disk, and the socket carried
    // those plus five entry headers.
    assert_eq!(send_diag.disk_bytes.load(Relaxed), payload);
    assert!(
        send_diag.socket_bytes.load(Relaxed) > payload,
        "socket total must include entry headers"
    );
    assert!(send_diag.walk_nanos.load(Relaxed) > 0);

    // Receiver: the symlink target and the three v2 digests cross the socket
    // but are never written as file data, so the two sides of the pipeline
    // differ by exactly those.
    assert_eq!(
        recv_diag.socket_bytes.load(Relaxed),
        payload + 3 * DIGEST_LEN as u64
    );
    assert_eq!(recv_diag.disk_bytes.load(Relaxed), on_disk);
    assert!(recv_diag.meta_nanos.load(Relaxed) > 0);

    // Verification is always on, so both ends spend time hashing.
    assert!(recv_diag.hash_nanos.load(Relaxed) > 0);
    assert!(send_diag.hash_nanos.load(Relaxed) > 0);
    assert_eq!(recv_diag.flush_nanos.load(Relaxed), 0);

    // What `--stats` actually prints; visible with --nocapture.
    send_diag.report(true);
    recv_diag.report(false);
}
