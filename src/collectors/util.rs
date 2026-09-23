// Low-level helpers mirroring lib/util.js: /proc + /sys reads, command runs
// with timeout, numeric helpers.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Read a small text file; None on any error (missing, EACCES, ...).
pub fn read_text(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

pub fn read_num(path: &str) -> Option<f64> {
    read_text(path).and_then(|t| t.trim().parse().ok())
}

pub fn exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

pub fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

pub fn round(v: f64, d: u32) -> f64 {
    let p = 10f64.powi(d as i32);
    (v * p).round() / p
}

/// Unix epoch milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Like util.js run(): exec with timeout; None on error/timeout.
pub fn run(cmd: &str, args: &[&str], timeout_ms: u64) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut pipe = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_millis(timeout_ms) {
                    let _ = child.kill();
                    let _ = reader.join();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = reader.join();
                return None;
            }
        }
    }
    let data = reader.join().ok()?;
    Some(String::from_utf8_lossy(&data).into_owned())
}
