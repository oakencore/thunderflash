use std::io::{self, Write};
use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use clap::{Parser, Subcommand};
use thunderflash::awake;
use thunderflash::iface::{self, Bridge};
use thunderflash::phrase;
use thunderflash::progress::{Diag, Progress};
use thunderflash::recv::Receiver;
use thunderflash::send;

const IFACE: &str = "bridge0";

#[derive(Parser)]
#[command(
    name = "tf",
    version,
    about = "Fast file transfer between two Macs over a Thunderbolt cable"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Wait for one incoming transfer, then exit.
    Recv {
        /// Where to write received files.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// TCP port. 0 picks any free port and advertises it via discovery.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Print transfer diagnostics to stderr when the run ends.
        #[arg(long)]
        stats: bool,
        /// Flush every received file to permanent storage before acknowledging.
        #[arg(long)]
        durable: bool,
    },
    /// Send files or directories to the Mac on the other end of the cable.
    Send {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Skip discovery and connect to this address directly.
        #[arg(long)]
        peer: Option<SocketAddrV4>,
        /// The three-word phrase shown by `tf recv`. Prompted for if omitted.
        #[arg(long)]
        token: Option<String>,
        /// Print transfer diagnostics to stderr when the run ends.
        #[arg(long)]
        stats: bool,
        /// Skip BLAKE3 content verification. Faster on links above ~2.5 GB/s;
        /// corruption can then arrive undetected. See the README.
        #[arg(long)]
        no_verify: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tf: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    match Cli::parse().command {
        Command::Recv {
            dir,
            port,
            stats,
            durable,
        } => receive(dir, port, stats, durable),
        Command::Send {
            paths,
            peer,
            token,
            stats,
            no_verify,
        } => transmit(paths, peer, token, stats, !no_verify),
    }
}

fn receive(dir: PathBuf, port: u16, stats: bool, durable: bool) -> io::Result<()> {
    let bridge = iface::find_bridge(IFACE)?;
    warn_about_mtu(&bridge);
    let _awake = keep_awake();

    // A fresh phrase every run: the receiver accepts exactly one connection,
    // so a wrong guess costs the attacker the whole session.
    let phrase = phrase::generate_phrase()?;
    let token = phrase::derive_token(&phrase);
    let diag = stats.then(|| Arc::new(Diag::default()));
    let mut receiver = Receiver::bind(&dir, bridge.addr, port, token)?.with_durable(durable);
    if let Some(diag) = &diag {
        receiver = receiver.with_diag(Arc::clone(diag));
    }
    let bound = receiver.port()?;

    std::thread::spawn(move || {
        if let Err(err) = iface::serve_discovery(bridge, bound) {
            eprintln!("tf: discovery responder stopped: {err}");
        }
    });

    eprintln!(
        "Listening on {}:{} — writing to {}",
        bridge.addr,
        bound,
        dir.display()
    );
    eprintln!("Phrase: {phrase}");

    let progress = Progress::new("receiving");
    let render = progress.start_render();
    let started = Instant::now();
    let result = receiver.accept_one(&progress);
    progress.finish();
    let _ = render.join();
    report(diag, started, false);

    result.map(|_| ())
}

fn transmit(
    paths: Vec<PathBuf>,
    peer: Option<SocketAddrV4>,
    token: Option<String>,
    stats: bool,
    verify: bool,
) -> io::Result<()> {
    let bridge = iface::find_bridge(IFACE)?;
    warn_about_mtu(&bridge);
    let _awake = keep_awake();

    let phrase = match token {
        Some(phrase) => phrase,
        None => prompt_for_phrase()?,
    };
    let token = phrase::derive_token(&phrase);
    let peer = match peer {
        Some(peer) => peer,
        None => iface::find_peer(&bridge)?,
    };

    let diag = stats.then(|| Arc::new(Diag::default()));
    let progress = Progress::new("sending");
    let render = progress.start_render();
    let started = Instant::now();
    let result = send::send_to_diag(peer, &paths, &token, &progress, verify, diag.clone());
    progress.finish();
    let _ = render.join();
    report(diag, started, true);

    result.map(|_| ())
}

/// Timed out here rather than inside the transfer so a failed run still
/// reports how long it ran for.
fn report(diag: Option<Arc<Diag>>, started: Instant, sending: bool) {
    if let Some(diag) = diag {
        diag.total_nanos
            .store(started.elapsed().as_nanos() as u64, Relaxed);
        diag.report(sending);
    }
}

fn prompt_for_phrase() -> io::Result<String> {
    eprint!("Phrase shown on the receiving Mac: ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a phrase is required (or pass --token <phrase>)",
        ));
    }
    Ok(line.to_string())
}

/// Both ends of a transfer run tf, so both Macs stay awake. Failure to
/// spawn caffeinate is a warning, not a reason to abort the transfer.
fn keep_awake() -> Option<awake::KeepAwake> {
    match awake::start() {
        Ok(guard) => Some(guard),
        Err(err) => {
            eprintln!("tf: could not prevent sleep (caffeinate: {err})");
            None
        }
    }
}

/// Jumbo frames measured no gain on a TB5-class pair, but may still help
/// older links. Changing network configuration is the user's call, not ours —
/// and macOS refuses an MTU on bridge0 until its members are raised first.
fn warn_about_mtu(bridge: &Bridge) {
    if bridge.mtu == Some(1500) {
        eprintln!(
            "hint: {IFACE} MTU is 1500. Jumbo frames may help older links: set mtu 9000 \
             on each member interface (ifconfig {IFACE} | grep member), then {IFACE}, on BOTH Macs."
        );
    }
}
