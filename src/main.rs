use std::io::{self, Write};
use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use thunderflash::awake;
use thunderflash::iface::{self, Bridge};
use thunderflash::phrase;
use thunderflash::progress::Progress;
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
        Command::Recv { dir, port } => receive(dir, port),
        Command::Send { paths, peer, token } => transmit(paths, peer, token),
    }
}

fn receive(dir: PathBuf, port: u16) -> io::Result<()> {
    let bridge = iface::find_bridge(IFACE)?;
    warn_about_mtu(&bridge);
    let _awake = keep_awake();

    // A fresh phrase every run: the receiver accepts exactly one connection,
    // so a wrong guess costs the attacker the whole session.
    let phrase = phrase::generate_phrase()?;
    let token = phrase::derive_token(&phrase);
    let receiver = Receiver::bind(&dir, bridge.addr, port, token)?;
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
    let result = receiver.accept_one(&progress);
    progress.finish();
    let _ = render.join();

    result.map(|_| ())
}

fn transmit(
    paths: Vec<PathBuf>,
    peer: Option<SocketAddrV4>,
    token: Option<String>,
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

    let progress = Progress::new("sending");
    let render = progress.start_render();
    let result = send::send_to(peer, &paths, &token, &progress);
    progress.finish();
    let _ = render.join();

    result.map(|_| ())
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

/// Jumbo frames are worth real throughput at multi-gigabit rates, but
/// changing network configuration is the user's call, not ours.
fn warn_about_mtu(bridge: &Bridge) {
    if bridge.mtu == Some(1500) {
        eprintln!(
            "hint: {IFACE} MTU is 1500. For more throughput, run this on BOTH Macs:\n  \
             sudo ifconfig {IFACE} mtu 9000"
        );
    }
}
