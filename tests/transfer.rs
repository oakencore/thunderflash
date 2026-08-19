use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use thunderflash::progress::Progress;
use thunderflash::recv::Receiver;
use thunderflash::wire::{Stats, TOKEN_LEN};
use thunderflash::{send, sys};

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

/// One receiver, one sender, both driven to completion. Returns what each end
/// counted, which is the only way to see the manifest phase from outside.
fn round_trip(paths: &[PathBuf], dest: &Path, token: [u8; TOKEN_LEN]) -> (Stats, Stats) {
    let receiver = Receiver::bind(dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });

    let progress = Progress::new("sending");
    let sent = send::send_to(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        paths,
        &token,
        &progress,
    )
    .unwrap();
    (sent, handle.join().unwrap())
}

fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The skip check compares mtimes exactly, so the source ones are pinned
/// rather than left at whatever second the test happened to run in.
fn set_mtime(path: &Path, secs: i64) {
    let dir = sys::open_dir(path.parent().unwrap()).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    sys::set_mtime_at(dir.as_fd(), name, secs).unwrap();
}

const PLAIN: [(&str, &str); 3] = [
    ("a.txt", "alpha"),
    ("sub/b.txt", "bravo two"),
    ("sub/c.txt", "charlie"),
];

/// No symlinks anywhere, so a pass in which every file is skipped moves no
/// bytes at all and the totals can be asserted exactly.
fn build_plain_tree(root: &Path) -> u64 {
    std::fs::create_dir_all(root.join("plain/sub")).unwrap();
    for (i, (name, body)) in PLAIN.iter().enumerate() {
        let path = root.join("plain").join(name);
        std::fs::write(&path, body).unwrap();
        set_mtime(&path, 1_600_000_000 + i as i64);
    }
    PLAIN.iter().map(|(_, body)| body.len() as u64).sum()
}

/// Every file already on the far side, byte for byte and second for second:
/// the second pass must move nothing but the directory entries.
#[test]
fn a_repeat_transfer_skips_every_unchanged_file() {
    let source = scratch("skip-all-source");
    let dest = scratch("skip-all-dest");
    let total = build_plain_tree(&source);
    let token = [61u8; TOKEN_LEN];
    let paths = [source.join("plain")];

    let (first, _) = round_trip(&paths, &dest, token);
    assert_eq!(first.skipped_files, 0, "a cold destination holds nothing");
    assert_eq!(first.bytes, total);

    let (sent, received) = round_trip(&paths, &dest, token);
    assert_eq!(sent.skipped_files, PLAIN.len() as u64);
    assert_eq!(sent.skipped_bytes, total);
    assert_eq!(sent.bytes, 0, "a fully skipped pass moves no file data");
    assert_eq!(sent.files, 2, "only the two directories are still sent");
    assert_eq!(received.skipped_files, sent.skipped_files);
    assert_eq!(received.skipped_bytes, sent.skipped_bytes);
    assert_eq!(received.bytes, 0);

    // Skipping must leave what is already there exactly as it was, or the next
    // pass would skip a file that is wrong.
    for (name, body) in PLAIN {
        let landed = dest.join("plain").join(name);
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), body, "{name}");
        assert_eq!(
            mtime_of(&landed),
            mtime_of(&source.join("plain").join(name)),
            "{name} lost its mtime"
        );
    }
}

/// The quick check is per file, not per tree: one changed file must travel and
/// its neighbours must not.
#[test]
fn only_the_file_that_changed_is_sent_again() {
    let source = scratch("skip-one-source");
    let dest = scratch("skip-one-dest");
    build_plain_tree(&source);
    let token = [62u8; TOKEN_LEN];
    let paths = [source.join("plain")];

    round_trip(&paths, &dest, token);

    let changed = source.join("plain/sub/b.txt");
    std::fs::write(&changed, "bravo two three").unwrap();
    set_mtime(&changed, 1_700_000_000);

    let (sent, received) = round_trip(&paths, &dest, token);
    assert_eq!(sent.skipped_files, 2, "the two untouched files must skip");
    assert_eq!(sent.bytes, "bravo two three".len() as u64);
    assert_eq!(received.bytes, sent.bytes);
    assert_eq!(
        std::fs::read_to_string(dest.join("plain/sub/b.txt")).unwrap(),
        "bravo two three"
    );
    assert_eq!(mtime_of(&dest.join("plain/sub/b.txt")), 1_700_000_000);
    assert_eq!(
        std::fs::read_to_string(dest.join("plain/a.txt")).unwrap(),
        "alpha"
    );
}

/// The bar promises what is left to move, not what was offered: the totals are
/// set only after the manifest phase has said which files stay put.
#[test]
fn the_senders_progress_totals_count_only_the_unskipped_files() {
    let source = scratch("totals-source");
    let dest = scratch("totals-dest");
    let total = build_plain_tree(&source);
    let token = [69u8; TOKEN_LEN];
    let paths = [source.join("plain")];

    round_trip(&paths, &dest, token);

    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });
    let progress = Progress::new("sending");
    send::send_to(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &paths,
        &token,
        &progress,
    )
    .unwrap();
    handle.join().unwrap();

    assert!(total > 0, "setup: the first pass must have moved something");
    assert_eq!(
        progress.totals(),
        (2, 0),
        "only the two directories are still on the sender's list"
    );
}

/// The receiver's scratch name must not be a name the sender can also send:
/// two inodes sharing one directory entry would lose one of them, silently and
/// with every digest still matching.
#[test]
fn a_source_file_named_like_a_partial_survives_alongside_its_twin() {
    let source = scratch("partial-name-source");
    let dest = scratch("partial-name-dest");
    let tree = source.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("x"), b"the real x").unwrap();
    std::fs::write(tree.join(".x.tf-partial"), b"the real dotfile").unwrap();

    round_trip(&[tree], &dest, [70u8; TOKEN_LEN]);

    assert_eq!(
        std::fs::read_to_string(dest.join("tree/x")).unwrap(),
        "the real x"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("tree/.x.tf-partial")).unwrap(),
        "the real dotfile"
    );
}

/// An interrupted transfer must not cost the destination the copy it already
/// had: the receiver writes to a temp name and renames only once the file is
/// complete and verified.
#[test]
fn an_interrupted_transfer_leaves_the_previous_copy_intact() {
    let source = scratch("interrupt-source");
    let dest = scratch("interrupt-dest");
    let file = source.join("keepme.bin");
    std::fs::write(&file, vec![7u8; 4 << 20]).unwrap();
    let previous = b"the copy that was already here";
    std::fs::write(dest.join("keepme.bin"), previous).unwrap();

    let token = [63u8; TOKEN_LEN];
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();

    // The sender reads nothing off disk until the manifest has been answered,
    // so while it waits for a receiver that has not accepted yet the file can
    // be shrunk under it with no timing to tune. It then streams what is left,
    // hits EOF early and aborts — mid-file, by construction.
    let (tx, aborted) = mpsc::channel();
    let sending = file.clone();
    std::thread::spawn(move || {
        let progress = Progress::new("sending");
        let _ = tx.send(send::send_to(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            &[sending],
            &token,
            &progress,
        ));
    });
    std::thread::sleep(Duration::from_millis(200));
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();

    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress)
    });

    let err = aborted
        .recv_timeout(Duration::from_secs(30))
        .expect("send_to must return")
        .expect_err("a shrinking file must not be padded into a success");
    assert!(err.to_string().contains("shrank"), "unhelpful error: {err}");
    let receiver_err = handle.join().unwrap().expect_err("receiver must not ack");
    assert!(
        receiver_err.to_string().contains("keepme.bin truncated"),
        "the receiver must have been mid-file, not stopped earlier: {receiver_err}"
    );

    assert_eq!(
        std::fs::read(dest.join("keepme.bin")).unwrap(),
        previous,
        "the previous copy must survive a failed transfer untouched"
    );
    let left: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "keepme.bin")
        .collect();
    assert!(left.is_empty(), "partial files left behind: {left:?}");
}

/// A name too long to prefix and suffix falls back to writing in place. It
/// still has to land, and still has to be skippable next time.
#[test]
fn names_at_the_length_limit_still_transfer() {
    let source = scratch("longname-source");
    let dest = scratch("longname-dest");
    std::fs::create_dir_all(source.join("long")).unwrap();

    // NAME_MAX is 255; ".{name}.tf-partial" costs another 12 characters, so
    // the first of these can be written through a temp name and the second
    // cannot.
    let names = ["a".repeat(255 - 12), "b".repeat(255)];
    for name in &names {
        let path = source.join("long").join(name);
        std::fs::write(&path, name.as_bytes()).unwrap();
        set_mtime(&path, 1_650_000_000);
    }

    let token = [64u8; TOKEN_LEN];
    let paths = [source.join("long")];
    let (sent, _) = round_trip(&paths, &dest, token);
    assert_eq!(sent.skipped_files, 0);
    for name in &names {
        let landed = dest.join("long").join(name);
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            name.as_bytes(),
            "a {}-character name did not arrive",
            name.len()
        );
        assert_eq!(mtime_of(&landed), 1_650_000_000);
    }

    let (again, _) = round_trip(&paths, &dest, token);
    assert_eq!(
        again.skipped_files,
        names.len() as u64,
        "the in-place fallback must stamp the mtime like any other write"
    );
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
