use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

    pub fn set_current(&self, path: &str) {
        if let Ok(mut current) = self.shared.current.lock() {
            current.clear();
            current.push_str(path);
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
