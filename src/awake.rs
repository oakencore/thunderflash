//! Keep the Mac awake for the duration of a transfer.

use std::io;
use std::process::{Child, Command, Stdio};

/// Holds a `caffeinate` child process. While it lives, the system will not
/// idle-sleep and the display will not sleep. The child watches our pid, so
/// the assertion is released when we exit, even via SIGKILL.
///
/// Note this cannot override lid-close sleep on battery power; no userspace
/// tool can.
pub struct KeepAwake {
    child: Child,
}

pub fn start() -> io::Result<KeepAwake> {
    let child = Command::new("/usr/bin/caffeinate")
        .arg("-di")
        .arg("-w")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(KeepAwake { child })
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn caffeinate_starts_and_stops_cleanly() {
        let awake = super::start().unwrap();
        // Dropping must kill and reap the child without hanging.
        drop(awake);
    }
}
