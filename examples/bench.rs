//! Localhost benchmark harness.
//!
//! The `tf` binary binds `bridge0` only, so a single-machine benchmark cannot
//! use it. This example drives the library directly over 127.0.0.1 with a
//! fixed token, which measures the software path (walk, framing, chunk loop,
//! file creation) without a cable in it. See docs/benchmarks.md.

use std::fs::File;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use clap::{Parser, Subcommand};
use thunderflash::progress::{Diag, Progress};
use thunderflash::recv::Receiver;
use thunderflash::send;
use thunderflash::wire::{CHUNK, TOKEN_LEN};

/// Both halves of a run are started by the same script, so the phrase
/// handshake would only add a prompt to automate around.
const TOKEN: [u8; TOKEN_LEN] = [42u8; TOKEN_LEN];

/// Fixture files per directory. Kept under a few thousand so readdir cost
/// does not quietly become the thing being measured.
const FILES_PER_DIR: usize = 2000;

/// Write buffer for fixture generation.
const FILL_BUF: usize = 8 << 20;

#[derive(Parser)]
#[command(name = "bench", about = "thunderflash localhost benchmark harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a fixture tree: large=<GiB>, small=<count>, medium=<count>, mixed, edge.
    Fix { workload: String, dir: PathBuf },
    /// Receive one transfer on 127.0.0.1, printing `PORT <n>` once bound.
    Recv {
        dir: PathBuf,
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Flush each file and the destination directory before acknowledging.
        #[arg(long)]
        durable: bool,
        /// Print the same diagnostics block as `tf recv --stats`.
        #[arg(long)]
        stats: bool,
    },
    /// Send to 127.0.0.1:<port>.
    Send {
        port: u16,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Send without BLAKE3 digests, as `tf send --no-verify` does.
        #[arg(long)]
        no_verify: bool,
        /// Print the same diagnostics block as `tf send --stats`.
        #[arg(long)]
        stats: bool,
    },
}

fn main() -> io::Result<()> {
    match Cli::parse().command {
        Command::Fix { workload, dir } => fixture(&workload, &dir),
        Command::Recv {
            dir,
            port,
            durable,
            stats,
        } => receive(dir, port, durable, stats),
        Command::Send {
            port,
            paths,
            no_verify,
            stats,
        } => transmit(port, paths, !no_verify, stats),
    }
}

/// xorshift64*, seeded per fixture. Output is incompressible, so a transfer
/// of it cannot be flattered by anything that special-cases zeroes, and it is
/// cheap enough that generation stays disk-bound.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for slot in buf.chunks_mut(8) {
            let word = self.next().to_le_bytes();
            slot.copy_from_slice(&word[..slot.len()]);
        }
    }
}

fn fixture(spec: &str, dir: &Path) -> io::Result<()> {
    // Regenerating 16 GiB on every run would dwarf the benchmark itself.
    if dir.is_dir() && dir.read_dir()?.next().is_some() {
        println!("fixture exists, reusing");
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (name, arg) = match spec.split_once('=') {
        Some((name, arg)) => (name, Some(arg)),
        None => (spec, None),
    };

    match (name, arg) {
        ("large", Some(gib)) => write_file(&dir.join("large.bin"), count(gib)? << 30, &mut rng),
        ("small", Some(n)) => spread(dir, count(n)? as usize, 4 << 10, &mut rng),
        ("medium", Some(n)) => spread(dir, count(n)? as usize, 1 << 20, &mut rng),
        ("mixed", None) => mixed(dir, &mut rng),
        ("edge", None) => edge(dir, &mut rng),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown workload {spec:?}"),
        )),
    }
}

fn count(text: &str) -> io::Result<u64> {
    text.parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("not a number: {text}")))
}

fn write_file(path: &Path, size: u64, rng: &mut Rng) -> io::Result<()> {
    let mut file = File::create(path)?;
    let mut buf = vec![0u8; (size.max(1) as usize).min(FILL_BUF)];
    let mut remaining = size;
    while remaining > 0 {
        let want = buf.len().min(remaining as usize);
        rng.fill(&mut buf[..want]);
        file.write_all(&buf[..want])?;
        remaining -= want as u64;
    }
    Ok(())
}

fn spread(dir: &Path, files: usize, size: u64, rng: &mut Rng) -> io::Result<()> {
    for i in 0..files {
        let sub = dir.join(format!("d{:03}", i / FILES_PER_DIR));
        if i % FILES_PER_DIR == 0 {
            std::fs::create_dir_all(&sub)?;
        }
        write_file(&sub.join(format!("f{i:06}.bin")), size, rng)?;
    }
    Ok(())
}

/// A tree where the bytes and the per-entry costs come from different files,
/// which is what a real ~/Movies or project directory looks like.
fn mixed(dir: &Path, rng: &mut Rng) -> io::Result<()> {
    std::fs::create_dir_all(dir.join("media"))?;
    for i in 0..4 {
        write_file(&dir.join(format!("media/clip{i}.mov")), 100 << 20, rng)?;
    }
    for d in 0..4 {
        let sub = dir.join(format!("docs/d{d}"));
        std::fs::create_dir_all(&sub)?;
        for i in 0..100 {
            write_file(&sub.join(format!("n{i:03}.txt")), 8 << 10, rng)?;
        }
    }
    std::fs::create_dir_all(dir.join("empty/deeper"))?;
    File::create(dir.join("docs/zero.bin"))?;
    File::create(dir.join("media/zero.bin"))?;
    std::os::unix::fs::symlink("../media/clip0.mov", dir.join("docs/link.mov"))?;
    // Dangling on purpose: symlinks are recorded, never followed.
    std::os::unix::fs::symlink("nowhere", dir.join("dangling"))
}

/// Sizes either side of the chunk boundary, where framing off-by-ones live.
fn edge(dir: &Path, rng: &mut Rng) -> io::Result<()> {
    let chunk = CHUNK as u64;
    write_file(&dir.join("chunk-exact.bin"), chunk, rng)?;
    write_file(&dir.join("chunk-minus-1.bin"), chunk - 1, rng)?;
    write_file(&dir.join("chunk-plus-1.bin"), chunk + 1, rng)?;
    File::create(dir.join("zero.bin"))?;
    std::fs::create_dir_all(dir.join("empty-dir"))?;
    std::fs::create_dir_all(dir.join("nested/empty"))?;
    File::create(dir.join("nested/zero.bin"))?;
    Ok(())
}

fn receive(dir: PathBuf, port: u16, durable: bool, want_stats: bool) -> io::Result<()> {
    let diag = want_stats.then(|| Arc::new(Diag::default()));
    let mut receiver =
        Receiver::bind(&dir, Ipv4Addr::LOCALHOST, port, TOKEN)?.with_durable(durable);
    if let Some(diag) = &diag {
        receiver = receiver.with_diag(Arc::clone(diag));
    }
    // The driver script polls this line out of the log to learn the port, so
    // it has to be flushed before we block in accept.
    println!("PORT {}", receiver.port()?);
    io::stdout().flush()?;

    let progress = Progress::new("receiving");
    // Includes the wait for the sender to connect; the summary table takes
    // throughput from the sender's clock for that reason.
    let started = Instant::now();
    let stats = receiver.accept_one(&progress)?;
    if let Some(diag) = &diag {
        diag.total_nanos
            .store(started.elapsed().as_nanos() as u64, Relaxed);
        diag.report(false);
    }
    println!(
        "RECV files={} bytes={} elapsed_s={:.3}",
        stats.files,
        stats.bytes,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn transmit(port: u16, paths: Vec<PathBuf>, verify: bool, want_stats: bool) -> io::Result<()> {
    // Always attached: the walk it times is the one inside the timed send, so
    // walk_s costs no second readdir pass and cannot warm the dentry cache
    // for the run it is measuring. Only the printed block is behind --stats.
    let diag = Arc::new(Diag::default());
    let progress = Progress::new("sending");
    let started = Instant::now();
    let stats = send::send_to_diag(
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        &paths,
        &TOKEN,
        &progress,
        verify,
        Some(Arc::clone(&diag)),
    )?;
    diag.total_nanos
        .store(started.elapsed().as_nanos() as u64, Relaxed);
    if want_stats {
        diag.report(true);
    }
    let walk_s = diag.walk_nanos.load(Relaxed) as f64 / 1e9;
    println!(
        "SEND files={} bytes={} elapsed_s={:.3} walk_s={walk_s:.3}",
        stats.files,
        stats.bytes,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
