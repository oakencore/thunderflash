use std::io::{self, Read};
use std::net::SocketAddrV4;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use thunderflash::iface::{self, Bridge};
use thunderflash::progress::Progress;
use thunderflash::recv::Receiver;
use thunderflash::send;
use thunderflash::wire::TOKEN_LEN;

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
        /// Override the stored token.
        #[arg(long)]
        token: Option<String>,
    },
    /// Show the local token, or store the one printed by the other Mac.
    Token { value: Option<String> },
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
        Command::Token { value } => match value {
            Some(value) => {
                store_token(&parse_token(&value)?)?;
                println!("token stored");
                Ok(())
            }
            None => {
                println!("{}", hex(&load_or_create_token()?));
                Ok(())
            }
        },
    }
}

fn receive(dir: PathBuf, port: u16) -> io::Result<()> {
    let bridge = iface::find_bridge(IFACE)?;
    warn_about_mtu(&bridge);

    let token = load_or_create_token()?;
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
    eprintln!("Token: {}", hex(&token));

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

    let token = match token {
        Some(value) => parse_token(&value)?,
        None => load_token()?,
    };
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

fn token_path() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".config/thunderflash/token"))
}

fn load_or_create_token() -> io::Result<[u8; TOKEN_LEN]> {
    match load_token() {
        Ok(token) => Ok(token),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let mut token = [0u8; TOKEN_LEN];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut token)?;
            store_token(&token)?;
            Ok(token)
        }
        Err(err) => Err(err),
    }
}

fn load_token() -> io::Result<[u8; TOKEN_LEN]> {
    let text = std::fs::read_to_string(token_path()?)?;
    parse_token(text.trim())
}

fn store_token(token: &[u8; TOKEN_LEN]) -> io::Result<()> {
    let path = token_path()?;
    let dir = path.parent().expect("token path always has a parent");
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(&path, hex(token))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn parse_token(text: &str) -> io::Result<[u8; TOKEN_LEN]> {
    let text = text.trim();
    if text.len() != TOKEN_LEN * 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("token must be {} hex characters", TOKEN_LEN * 2),
        ));
    }
    let mut token = [0u8; TOKEN_LEN];
    for (i, slot) in token.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "token is not valid hex"))?;
    }
    Ok(token)
}
