use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use thunderflash::progress::Progress;
use thunderflash::recv::Receiver;
use thunderflash::send;
use thunderflash::wire::{CHUNK, HANDSHAKE_LEN, TOKEN_LEN};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tf-pool-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Bigger than everything the sender can swallow without the receiver
/// draining: the buffer pool (~24 MB) plus the 8 MB socket send buffer plus
/// the peer's receive buffer. A file this size guarantees the sender is still
/// mid-read when we stall it, with no sleeps to tune.
const BIGGER_THAN_ALL_BUFFERS: usize = 96 * 1024 * 1024;

fn send_in_background(
    port: u16,
    paths: Vec<PathBuf>,
    token: [u8; TOKEN_LEN],
) -> mpsc::Receiver<std::io::Result<thunderflash::wire::Stats>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let progress = Progress::new("sending");
        let _ = tx.send(send::send_to(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            &paths,
            &token,
            &progress,
        ));
    });
    rx
}

#[test]
fn a_multi_chunk_file_round_trips_byte_identically() {
    let source = scratch("multi-src");
    let dest = scratch("multi-dst");
    let data = pattern(CHUNK * 3 + 12_345);
    std::fs::write(source.join("big.bin"), &data).unwrap();

    let token = [1u8; TOKEN_LEN];
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });

    let progress = Progress::new("sending");
    let sent = send::send_to(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &[source.join("big.bin")],
        &token,
        &progress,
    )
    .unwrap();
    let received = handle.join().unwrap();

    assert_eq!(sent.bytes, data.len() as u64);
    assert_eq!(sent.bytes, received.bytes);
    assert_eq!(std::fs::read(dest.join("big.bin")).unwrap(), data);
}

#[test]
fn chunk_boundary_and_empty_files_round_trip() {
    let source = scratch("edge-src");
    let dest = scratch("edge-dst");
    let sizes = [0usize, 1, CHUNK - 1, CHUNK, CHUNK + 1];
    std::fs::create_dir_all(source.join("edge")).unwrap();
    for size in sizes {
        std::fs::write(source.join(format!("edge/f{size}.bin")), pattern(size)).unwrap();
    }

    let token = [2u8; TOKEN_LEN];
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();
    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress).unwrap()
    });

    let progress = Progress::new("sending");
    send::send_to(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &[source.join("edge")],
        &token,
        &progress,
    )
    .unwrap();
    handle.join().unwrap();

    for size in sizes {
        let name = format!("edge/f{size}.bin");
        assert_eq!(
            std::fs::read(dest.join(&name)).unwrap(),
            pattern(size),
            "mismatch in {name}"
        );
    }
}

#[test]
fn a_file_that_shrank_aborts_the_send_and_leaves_no_partial() {
    let source = scratch("shrink-src");
    let dest = scratch("shrink-dst");
    let file = source.join("shrinking.bin");
    std::fs::write(&file, pattern(BIGGER_THAN_ALL_BUFFERS)).unwrap();

    let token = [3u8; TOKEN_LEN];
    let receiver = Receiver::bind(&dest, Ipv4Addr::LOCALHOST, 0, token).unwrap();
    let port = receiver.port().unwrap();

    // Deliberately do NOT accept yet: the sender fills its pool and the
    // socket buffers, then blocks in write_all. It cannot have read the whole
    // file, so truncating now is guaranteed to be observed mid-transfer.
    let sent = send_in_background(port, vec![file.clone()], token);
    std::thread::sleep(Duration::from_millis(200));
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(0)
        .unwrap();

    let handle = std::thread::spawn(move || {
        let progress = Progress::new("receiving");
        receiver.accept_one(&progress)
    });

    let err = sent
        .recv_timeout(Duration::from_secs(30))
        .expect("send_to must return")
        .expect_err("a shrinking file must not be padded into a success");
    assert!(err.to_string().contains("shrank"), "unhelpful error: {err}");

    assert!(handle.join().unwrap().is_err(), "receiver must not ack");
    assert!(
        !dest.join("shrinking.bin").exists(),
        "the partial file must be deleted"
    );
}

#[test]
fn a_receiver_that_hangs_up_mid_transfer_ends_the_send_promptly() {
    let source = scratch("hangup-src");
    let file = source.join("big.bin");
    std::fs::write(&file, pattern(BIGGER_THAN_ALL_BUFFERS)).unwrap();

    let token = [4u8; TOKEN_LEN];
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let sent = send_in_background(port, vec![file], token);

    // Read just enough to get the transfer moving, then hang up hard.
    let (mut stream, _) = listener.accept().unwrap();
    let mut scratch_buf = [0u8; 64 * 1024];
    stream
        .read_exact(&mut scratch_buf[..HANDSHAKE_LEN])
        .unwrap();
    let _ = stream.read(&mut scratch_buf);
    let _ = stream.write_all(&[]);
    drop(stream);

    let result = sent
        .recv_timeout(Duration::from_secs(30))
        .expect("send_to must return rather than hang");
    assert!(result.is_err(), "a hung-up receiver must fail the send");
}
