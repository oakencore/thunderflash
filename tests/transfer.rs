use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use thunderflash::progress::Progress;
use thunderflash::recv::Receiver;
use thunderflash::send;
use thunderflash::wire::TOKEN_LEN;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tf-e2e-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A tree covering both workloads the tool is for: one file larger than the
/// 4 MB chunk size, and a few thousand small ones.
fn build_tree(root: &Path) {
    std::fs::create_dir_all(root.join("tree/big")).unwrap();
    std::fs::create_dir_all(root.join("tree/small")).unwrap();
    std::fs::create_dir_all(root.join("tree/empty")).unwrap();

    let big: Vec<u8> = (0..10_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.join("tree/big/large.bin"), &big).unwrap();

    for i in 0..2000 {
        std::fs::write(
            root.join(format!("tree/small/f{i:04}.txt")),
            format!("file {i}"),
        )
        .unwrap();
    }

    std::fs::write(root.join("tree/zero.bin"), b"").unwrap();
    std::os::unix::fs::symlink("big/large.bin", root.join("tree/link")).unwrap();

    let exe = root.join("tree/script.sh");
    std::fs::write(&exe, b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn transfers_a_tree_over_loopback() {
    let source = scratch("source");
    let dest = scratch("dest");
    build_tree(&source);

    let token = [42u8; TOKEN_LEN];
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();

    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });

    let progress = Progress::new("sending");
    let sent = send::send_to(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &[source.join("tree")],
        &token,
        &progress,
    )
    .unwrap();
    let received = handle.join().unwrap();

    assert_eq!(
        sent.files, received.files,
        "sender and receiver disagree on file count"
    );
    assert_eq!(
        sent.bytes, received.bytes,
        "sender and receiver disagree on byte count"
    );

    // Large file arrives byte-identical.
    assert_eq!(
        std::fs::read(source.join("tree/big/large.bin")).unwrap(),
        std::fs::read(dest.join("tree/big/large.bin")).unwrap(),
    );

    // Every small file arrives with the right contents.
    for i in 0..2000 {
        let name = format!("tree/small/f{i:04}.txt");
        assert_eq!(
            std::fs::read_to_string(dest.join(&name)).unwrap(),
            format!("file {i}"),
            "mismatch in {name}"
        );
    }

    // Empty file, empty directory, and symlink all survive.
    assert_eq!(std::fs::read(dest.join("tree/zero.bin")).unwrap().len(), 0);
    assert!(dest.join("tree/empty").is_dir());
    assert_eq!(
        std::fs::read_link(dest.join("tree/link"))
            .unwrap()
            .to_str()
            .unwrap(),
        "big/large.bin"
    );

    // Executable bit is preserved.
    let mode = std::fs::metadata(dest.join("tree/script.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);

    // mtime is preserved within a second.
    let src_time = std::fs::metadata(source.join("tree/big/large.bin"))
        .unwrap()
        .modified()
        .unwrap();
    let dst_time = std::fs::metadata(dest.join("tree/big/large.bin"))
        .unwrap()
        .modified()
        .unwrap();
    let delta = src_time
        .duration_since(dst_time)
        .or_else(|_| dst_time.duration_since(src_time))
        .unwrap();
    assert!(delta.as_secs() <= 1, "mtime drifted by {delta:?}");
}

#[test]
fn a_second_transfer_over_the_same_destination_succeeds() {
    let source = scratch("resend-source");
    let dest = scratch("resend-dest");
    build_tree(&source);
    let token = [11u8; TOKEN_LEN];

    for round in 0..2 {
        let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
        let port = receiver.port().unwrap();
        let handle = std::thread::spawn(move || {
            let progress = Progress::new("receiving");
            receiver.accept_one(&progress).unwrap()
        });

        let progress = Progress::new("sending");
        send::send_to(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            &[source.join("tree")],
            &token,
            &progress,
        )
        .unwrap_or_else(|err| panic!("round {round} failed: {err}"));
        handle.join().unwrap();
    }

    assert_eq!(
        std::fs::read(source.join("tree/big/large.bin")).unwrap(),
        std::fs::read(dest.join("tree/big/large.bin")).unwrap(),
    );
}

/// `--no-verify` changes what crosses the wire, not what lands on disk.
#[test]
fn transfers_a_tree_without_verification() {
    let source = scratch("no-verify-source");
    let dest = scratch("no-verify-dest");
    build_tree(&source);

    let token = [43u8; TOKEN_LEN];
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });

    let progress = Progress::new("sending");
    let sent = send::send_to_diag(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &[source.join("tree")],
        &token,
        &progress,
        false,
        None,
    )
    .unwrap();
    let received = handle.join().unwrap();

    assert_eq!(sent.files, received.files);
    assert_eq!(sent.bytes, received.bytes);
    assert_eq!(
        std::fs::read(source.join("tree/big/large.bin")).unwrap(),
        std::fs::read(dest.join("tree/big/large.bin")).unwrap(),
    );
    for i in [0, 999, 1999] {
        let name = format!("tree/small/f{i:04}.txt");
        assert_eq!(
            std::fs::read_to_string(dest.join(&name)).unwrap(),
            format!("file {i}"),
            "mismatch in {name}"
        );
    }
    assert_eq!(std::fs::read(dest.join("tree/zero.bin")).unwrap().len(), 0);
    assert!(dest.join("tree/empty").is_dir());
    assert_eq!(
        std::fs::read_link(dest.join("tree/link"))
            .unwrap()
            .to_str()
            .unwrap(),
        "big/large.bin"
    );
}

/// `--durable` changes when the receiver acknowledges, not what it writes.
#[test]
fn transfers_a_tree_with_durable_flushes() {
    let source = scratch("durable-source");
    let dest = scratch("durable-dest");
    build_tree(&source);

    let token = [23u8; TOKEN_LEN];
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token)
        .unwrap()
        .with_durable(true);
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });

    let progress = Progress::new("sending");
    let sent = send::send_to(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &[source.join("tree")],
        &token,
        &progress,
    )
    .unwrap();
    let received = handle.join().unwrap();

    assert_eq!(sent.files, received.files);
    assert_eq!(sent.bytes, received.bytes);
    assert_eq!(
        std::fs::read(source.join("tree/big/large.bin")).unwrap(),
        std::fs::read(dest.join("tree/big/large.bin")).unwrap(),
    );
    // A zero-byte file is flushed like any other; a symlink is never flushed.
    assert_eq!(std::fs::read(dest.join("tree/zero.bin")).unwrap().len(), 0);
    assert_eq!(
        std::fs::read_link(dest.join("tree/link"))
            .unwrap()
            .to_str()
            .unwrap(),
        "big/large.bin"
    );
}

/// Nothing to fsync at all: the flush path must not assume a file was opened.
#[test]
fn durable_survives_a_symlink_only_transfer() {
    let source = scratch("durable-links-source");
    let dest = scratch("durable-links-dest");
    std::fs::create_dir_all(source.join("links")).unwrap();
    std::os::unix::fs::symlink("nowhere", source.join("links/dangling")).unwrap();
    std::os::unix::fs::symlink("../elsewhere", source.join("links/other")).unwrap();

    let token = [24u8; TOKEN_LEN];
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token)
        .unwrap()
        .with_durable(true);
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });

    let progress = Progress::new("sending");
    send::send_to(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &[source.join("links")],
        &token,
        &progress,
    )
    .unwrap();
    handle.join().unwrap();

    assert_eq!(
        std::fs::read_link(dest.join("links/dangling"))
            .unwrap()
            .to_str()
            .unwrap(),
        "nowhere"
    );
    assert!(dest.join("links/other").is_symlink());
}

/// Read-only files and directories must transfer, and must still transfer the
/// second time over a destination that already holds the read-only copies.
#[test]
fn read_only_files_and_directories_survive_a_retransfer() {
    let source = scratch("readonly-src");
    let dest = scratch("readonly-dst");
    std::fs::create_dir_all(source.join("tree/locked")).unwrap();
    std::fs::write(source.join("tree/locked/report.txt"), b"final").unwrap();
    std::fs::set_permissions(
        source.join("tree/locked/report.txt"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    std::fs::set_permissions(
        source.join("tree/locked"),
        std::fs::Permissions::from_mode(0o555),
    )
    .unwrap();

    let landed = dest.join("tree/locked/report.txt");
    for pass in 1..=2 {
        let token = [51u8; TOKEN_LEN];
        let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
        let port = receiver.port().unwrap();
        let handle = std::thread::spawn(move || {
            let progress = Progress::new("receiving");
            receiver.accept_one(&progress)
        });
        let progress = Progress::new("sending");
        send::send_to(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            &[source.join("tree")],
            &token,
            &progress,
        )
        .unwrap_or_else(|err| panic!("pass {pass} failed to send: {err}"));
        handle
            .join()
            .unwrap()
            .unwrap_or_else(|err| panic!("pass {pass} failed to receive: {err}"));

        assert_eq!(std::fs::read(&landed).unwrap(), b"final", "pass {pass}");
        assert_eq!(
            std::fs::metadata(&landed).unwrap().permissions().mode() & 0o777,
            0o444,
            "pass {pass}: the sender's mode must be applied"
        );
    }

    // The destination directory is left writable on purpose, so a retry can
    // replace what is inside it.
    let dir_mode = std::fs::metadata(dest.join("tree/locked"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o700, 0o700, "got {dir_mode:o}");
}
