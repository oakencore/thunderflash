use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_millis(200);

struct Shared {
    verb: &'static str,
    bytes: AtomicU64,
    files: AtomicU64,
    total_bytes: AtomicU64,
    total_files: AtomicU64,
    current: Mutex<String>,
    done: AtomicBool,
    started: Instant,
}

#[derive(Clone)]
pub struct Progress {
    shared: Arc<Shared>,
}

impl Progress {
    pub fn new(verb: &'static str) -> Progress {
        Progress {
            shared: Arc::new(Shared {
                verb,
                bytes: AtomicU64::new(0),
                files: AtomicU64::new(0),
                total_bytes: AtomicU64::new(0),
                total_files: AtomicU64::new(0),
                current: Mutex::new(String::new()),
                done: AtomicBool::new(false),
                started: Instant::now(),
            }),
        }
    }

    pub fn set_totals(&self, files: u64, bytes: u64) {
        self.shared.total_files.store(files, Ordering::Relaxed);
        self.shared.total_bytes.store(bytes, Ordering::Relaxed);
    }

    /// The path here comes off the wire on the receiver and off the disk on
    /// the sender, and lands on a terminal (or in a log) unquoted. Control
    /// characters are dropped so a crafted filename cannot move the cursor,
    /// repaint the line, or forge log entries.
    pub fn set_current(&self, path: &str) {
        if let Ok(mut current) = self.shared.current.lock() {
            current.clear();
            current.extend(path.chars().map(|c| if c.is_control() { '?' } else { c }));
        }
    }

    pub fn current(&self) -> String {
        self.shared
            .current
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    pub fn add_bytes(&self, n: u64) {
        self.shared.bytes.fetch_add(n, Ordering::Relaxed);
    }

    pub fn finish_file(&self) {
        self.shared.files.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.shared.files.load(Ordering::Relaxed),
            self.shared.bytes.load(Ordering::Relaxed),
        )
    }

    pub fn start_render(&self) -> JoinHandle<()> {
        let shared = Arc::clone(&self.shared);
        thread::spawn(move || render_loop(shared))
    }

    pub fn finish(&self) {
        self.shared.done.store(true, Ordering::Relaxed);
    }
}

fn is_tty() -> bool {
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

pub fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1e9;
    const MB: f64 = 1e6;
    const KB: f64 = 1e3;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.2} GB", value / GB)
    } else if value >= MB {
        format!("{:.2} MB", value / MB)
    } else if value >= KB {
        format!("{:.2} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_rate(bytes_per_second: f64) -> String {
    const GB: f64 = 1e9;
    const MB: f64 = 1e6;
    if bytes_per_second >= GB {
        format!("{:.2} GB/s", bytes_per_second / GB)
    } else if bytes_per_second >= MB {
        format!("{:.2} MB/s", bytes_per_second / MB)
    } else {
        format!("{:.2} KB/s", bytes_per_second / 1e3)
    }
}

fn render_loop(shared: Arc<Shared>) {
    let tty = is_tty();
    let mut last_bytes = 0u64;
    let mut last_at = Instant::now();
    // Non-TTY output would flood a log file at 5 Hz.
    let mut ticks_until_line = 0u32;

    loop {
        thread::sleep(REFRESH);
        let done = shared.done.load(Ordering::Relaxed);

        let bytes = shared.bytes.load(Ordering::Relaxed);
        let files = shared.files.load(Ordering::Relaxed);
        let total_files = shared.total_files.load(Ordering::Relaxed);

        let now = Instant::now();
        let instant_rate = (bytes - last_bytes) as f64 / now.duration_since(last_at).as_secs_f64();
        let average_rate = bytes as f64 / now.duration_since(shared.started).as_secs_f64();
        last_bytes = bytes;
        last_at = now;

        let current = shared.current.lock().map(|c| c.clone()).unwrap_or_default();
        // One line, so redrawing is a carriage return and an erase-to-end.
        // Multi-line redraw needs cursor-up sequences that break the moment
        // the terminal wraps a long path.
        let line = format!(
            "{} {}/{} files  {}  {}  (avg {})  {}",
            shared.verb,
            files,
            total_files,
            format_bytes(bytes),
            format_rate(instant_rate),
            format_rate(average_rate),
            truncate_middle(&current, 40),
        );

        let mut stderr = std::io::stderr().lock();
        if tty {
            let _ = write!(stderr, "\r\x1b[K{line}");
        } else if ticks_until_line == 0 {
            let _ = writeln!(stderr, "{line}");
            ticks_until_line = 10;
        } else {
            ticks_until_line -= 1;
        }
        let _ = stderr.flush();

        if done {
            let elapsed = now.duration_since(shared.started).as_secs_f64().max(1e-6);
            if tty {
                let _ = write!(stderr, "\r\x1b[K");
            }
            let _ = writeln!(
                stderr,
                "{} {} in {} files, {:.1}s ({})",
                shared.verb,
                format_bytes(bytes),
                files,
                elapsed,
                format_rate(bytes as f64 / elapsed),
            );
            let _ = stderr.flush();
            return;
        }
    }
}

/// Optional per-run diagnostics behind `--stats`, shared across threads like
/// `Progress`. Every counter is touched once per chunk or per entry, never per
/// byte, so leaving it attached costs nothing measurable.
///
/// New timings belong here as another `AtomicU64` plus a line in `report`.
#[derive(Default)]
pub struct Diag {
    pub disk_bytes: AtomicU64,
    pub socket_bytes: AtomicU64,
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub symlinks: AtomicU64,
    pub pool_stalls: AtomicU64,
    pub queue_peak: AtomicU64,
    /// Sender only: time spent building the transfer list.
    pub walk_nanos: AtomicU64,
    /// Receiver only: cumulative sanitize/walk_dirs/create/mkdir/symlink time.
    pub meta_nanos: AtomicU64,
    /// Receiver only: time spent in accept, waiting for a sender to connect.
    /// Subtracted from `total`, which is otherwise mostly idle time.
    pub accept_nanos: AtomicU64,
    pub total_nanos: AtomicU64,
    /// Reported only once later work starts filling them in.
    pub hash_nanos: AtomicU64,
    pub flush_nanos: AtomicU64,
    inflight: AtomicU64,
}

impl Diag {
    /// A chunk entered the queue. `fetch_max` keeps the peak lock-free.
    pub fn queued(&self) {
        let depth = self.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        self.queue_peak.fetch_max(depth, Ordering::Relaxed);
    }

    pub fn dequeued(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Compact block on stderr. `sending` chooses the direction labels; one
    /// counter is not symmetric — the sender's `socket_bytes` includes entry
    /// headers, the receiver's is payload plus digests only, since its decode
    /// path has no `Diag` in scope. They differ by the framing bytes.
    pub fn report(&self, sending: bool) {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        // Labels follow the counters, not the direction: `disk_bytes` is
        // always printed first.
        let (side, disk, socket) = match sending {
            true => ("send", "disk read", "socket out"),
            false => ("recv", "disk written", "socket in"),
        };
        let mut out = std::io::stderr().lock();
        let _ = writeln!(out, "stats [{side}]");
        let _ = writeln!(out, "  {disk:<12} {}", format_bytes(get(&self.disk_bytes)));
        let _ = writeln!(
            out,
            "  {socket:<12} {}",
            format_bytes(get(&self.socket_bytes))
        );
        let _ = writeln!(
            out,
            "  {:<12} files {}, dirs {}, symlinks {}",
            "entries",
            get(&self.files),
            get(&self.dirs),
            get(&self.symlinks)
        );
        let _ = writeln!(
            out,
            "  {:<12} {}  (queue peak {})",
            "pool stalls",
            get(&self.pool_stalls),
            get(&self.queue_peak)
        );
        let (phase, nanos) = match sending {
            true => ("walk", get(&self.walk_nanos)),
            false => ("metadata", get(&self.meta_nanos)),
        };
        let _ = writeln!(out, "  {phase:<12} {}", format_nanos(nanos));
        for (label, nanos) in [
            ("hash", get(&self.hash_nanos)),
            ("flush", get(&self.flush_nanos)),
            ("waiting", get(&self.accept_nanos)),
        ] {
            if nanos > 0 {
                let _ = writeln!(out, "  {label:<12} {}", format_nanos(nanos));
            }
        }
        // The receiver's clock starts before accept, so the wait for a sender
        // to connect would otherwise be reported as transfer time.
        let _ = writeln!(
            out,
            "  {:<12} {}",
            "total",
            format_nanos(get(&self.total_nanos).saturating_sub(get(&self.accept_nanos)))
        );
        let _ = out.flush();
    }
}

/// Accumulate an elapsed interval into a nanosecond counter.
pub fn add_elapsed(counter: &AtomicU64, since: Instant) {
    counter.fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// Take a buffer from a pool, counting the wait when none is free. `try_recv`
/// first so the uncontended case stays a plain channel pop.
pub fn pooled_recv(pool: &mpsc::Receiver<Vec<u8>>, diag: Option<&Diag>) -> Option<Vec<u8>> {
    if let Ok(buf) = pool.try_recv() {
        return Some(buf);
    }
    if let Some(diag) = diag {
        diag.pool_stalls.fetch_add(1, Ordering::Relaxed);
    }
    pool.recv().ok()
}

pub fn format_nanos(nanos: u64) -> String {
    match nanos {
        n if n >= 1_000_000_000 => format!("{:.2} s", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1} ms", n as f64 / 1e6),
        n => format!("{:.1} µs", n as f64 / 1e3),
    }
}

/// Keep a long path readable on one line by eliding its middle.
fn truncate_middle(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    let keep = width.saturating_sub(1) / 2;
    let head: String = chars[..keep].iter().collect();
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_across_clones() {
        let progress = Progress::new("sending");
        let clone = progress.clone();
        progress.add_bytes(100);
        clone.add_bytes(50);
        clone.finish_file();

        assert_eq!(progress.snapshot(), (1, 150));
    }

    #[test]
    fn current_path_is_readable_after_being_set() {
        let progress = Progress::new("sending");
        progress.set_current("a/b/c.txt");
        assert_eq!(progress.current(), "a/b/c.txt");
    }

    /// Paths arrive from a peer and are printed to a terminal unquoted, so a
    /// name carrying an escape sequence must not reach it.
    #[test]
    fn control_characters_in_a_path_never_reach_the_terminal() {
        let progress = Progress::new("receiving");
        progress.set_current("a\x1b[2Kb\r\nc\u{7}");
        let current = progress.current();
        assert!(!current.chars().any(|c| c.is_control()), "got {current:?}");
        assert_eq!(current, "a?[2Kb??c?");
    }

    #[test]
    fn rate_line_formats_bytes_and_throughput() {
        assert_eq!(format_rate(1_500_000_000.0), "1.50 GB/s");
        assert_eq!(format_rate(12_000_000.0), "12.00 MB/s");
        assert_eq!(format_bytes(47_200_000_000), "47.20 GB");
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn long_paths_are_elided_in_the_middle() {
        assert_eq!(truncate_middle("short.txt", 40), "short.txt");
        let long = "a".repeat(60);
        let out = truncate_middle(&long, 20);
        assert!(
            out.chars().count() <= 20,
            "got {} chars",
            out.chars().count()
        );
        assert!(out.contains('…'));
    }

    #[test]
    fn queue_peak_keeps_the_deepest_moment_not_the_last() {
        let diag = Diag::default();
        diag.queued();
        diag.queued();
        diag.queued();
        diag.dequeued();
        diag.dequeued();
        diag.queued();
        assert_eq!(diag.queue_peak.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_stall_is_counted_only_when_the_pool_is_empty() {
        let diag = Diag::default();
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);

        tx.send(vec![1u8; 4]).unwrap();
        assert!(pooled_recv(&rx, Some(&diag)).is_some());
        assert_eq!(diag.pool_stalls.load(Ordering::Relaxed), 0);

        // Nothing free yet, so this caller has to wait for the other thread.
        let late = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.send(vec![2u8; 4]).unwrap();
        });
        assert_eq!(pooled_recv(&rx, Some(&diag)), Some(vec![2u8; 4]));
        assert_eq!(diag.pool_stalls.load(Ordering::Relaxed), 1);
        late.join().unwrap();
    }

    #[test]
    fn durations_are_reported_in_readable_units() {
        assert_eq!(format_nanos(1_500_000_000), "1.50 s");
        assert_eq!(format_nanos(2_400_000), "2.4 ms");
        assert_eq!(format_nanos(900), "0.9 µs");
    }

    #[test]
    fn render_thread_starts_and_stops_cleanly() {
        let progress = Progress::new("sending");
        progress.set_totals(1, 10);
        let handle = progress.start_render();
        progress.add_bytes(10);
        progress.finish_file();
        progress.finish();
        handle.join().unwrap();
    }
}
