//! Spike probe: how many parallel TCP flows does this link sustain?
//!
//! Before trusting `TF_STREAMS` on a given cable, measure the raw ceiling: if
//! N parallel flows do not beat one by a wide margin, multi-stream is not
//! worth the extra memory on that link.
//!
//! A measurement instrument, not a feature: no framing, no verification, no
//! file semantics. Incompressible payload from a pre-filled buffer over N
//! independent connections, each with the same socket options `tf` uses.
//!
//!   streams_probe recv 172.10.0.1 6000 4     the receiving Mac
//!   streams_probe send 172.10.0.1 6000 4 16  the sending Mac
//!
//! `recv` opens N listeners on port..port+N-1 and drains each to EOF;
//! `send` pushes <gib> GiB total, split evenly, then closes each flow.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const BUF: usize = 8 * 1024 * 1024;

fn set_socket(stream: &TcpStream) -> io::Result<()> {
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        let bytes: libc::c_int = BUF as libc::c_int;
        let rc = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                std::ptr::addr_of!(bytes).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Fill with a cheap incompressible pattern, once.
fn fill() -> Vec<u8> {
    let mut out = vec![0u8; BUF];
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut slot = 0;
    while slot < BUF {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let word = x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        for b in word {
            if slot >= BUF {
                break;
            }
            out[slot] = b;
            slot += 1;
        }
    }
    out
}

fn recv_side(bind: IpAddr, base: u16, n: u16) -> io::Result<()> {
    let listeners = (0..n)
        .map(|i| TcpListener::bind((bind, base.saturating_add(i))))
        .collect::<io::Result<Vec<_>>>()?;
    let port = |l: &TcpListener| l.local_addr().unwrap().port();
    for (i, l) in listeners.iter().enumerate() {
        println!("STREAM {} listening on :{}", i, port(l));
    }
    println!("ready");

    let total = std::sync::Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    let handles: Vec<_> = listeners
        .into_iter()
        .enumerate()
        .map(|(i, listener)| {
            let total = std::sync::Arc::clone(&total);
            std::thread::spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = set_socket(&stream);
                let mut buf = vec![0u8; BUF];
                let mut got = 0u64;
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => got += k as u64,
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(err) => {
                            eprintln!("stream {i} died: {err}");
                            break;
                        }
                    }
                }
                println!("STREAM {i} got {got} bytes");
                total.fetch_add(got, Ordering::Relaxed);
            })
        })
        .collect();
    for h in handles {
        h.join().ok();
    }
    let bytes = total.load(Ordering::Relaxed); // via Arc, still a reference
    let secs = started.elapsed().as_secs_f64().max(1e-9);
    println!(
        "TOTAL {bytes} bytes in {secs:.3}s = {:.2} GB/s over {n} streams",
        bytes as f64 / secs / 1e9
    );
    Ok(())
}

fn send_side(addr: Ipv4Addr, base: u16, n: u16, gib: f64) -> io::Result<()> {
    let data = fill();
    let per = (gib * 1e9 / n as u64 as f64) as u64;
    let started = Instant::now();
    let mut handles = Vec::new();
    for i in 0..n {
        let peer = SocketAddr::new(addr.into(), base.saturating_add(i));
        let data = data.clone();
        handles.push(std::thread::spawn(move || {
            let mut stream =
                TcpStream::connect(peer).unwrap_or_else(|e| panic!("connect to {peer}: {e}"));
            let _ = set_socket(&stream);
            let _ = stream.set_nodelay(true);
            let mut sent = 0u64;
            while sent < per {
                let want = (per - sent).min(BUF as u64) as usize;
                stream
                    .write_all(&data[..want])
                    .unwrap_or_else(|e| panic!("write on stream {i}: {e}"));
                sent += want as u64;
            }
            // EOF is how the receiver learns the stream is done.
            let _ = stream.shutdown(std::net::Shutdown::Write);
            println!("STREAM {i} sent {sent} bytes");
        }));
    }
    for h in handles {
        h.join().ok();
    }
    let secs = started.elapsed();
    let total = per * n as u64;
    println!(
        "TOTAL {total} bytes in {:.3}s = {:.2} GB/s over {n} streams",
        secs.as_secs_f64(),
        total as f64 / secs.as_secs_f64() / 1e9
    );
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("recv") => {
            let bind: IpAddr = args[1].parse().expect("invalid bind address");
            let base: u16 = args[2].parse().expect("invalid port");
            let n: u16 = args[3].parse().expect("invalid stream count");
            recv_side(bind, base, n)
        }
        Some("send") => {
            let addr: Ipv4Addr = args[1].parse().expect("invalid address");
            let base: u16 = args[2].parse().expect("invalid port");
            let n: u16 = args[3].parse().expect("invalid stream count");
            let gib: f64 = args[4].parse().expect("invalid GiB");
            send_side(addr, base, n, gib)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: streams_probe recv <bind> <port> <N> | send <addr> <port> <N> <gib>",
        )),
    }
}
