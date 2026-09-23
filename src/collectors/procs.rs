// Process list sampler — mirrors lib/processes.js:
// /proc enumeration, per-process CPU (jiffy deltas vs total machine jiffies),
// RSS from /proc/<pid>/status, cmdline cached per pid, disk I/O for own uid,
// batched username resolution, dead-pid cache cleanup.
//
// Like the JS version, status and cmdline are read in parallel threads per
// pid (Promise.all equivalent); warm sampling over ~600 pids stays well under
// one 500 ms tick.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::collectors::util::{clamp, read_text, run};

fn self_uid() -> i64 {
    unsafe { libc::getuid() as i64 }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub state: char,
    pub uid: i64,
    pub user: String,
    pub cpu: f64,
    pub mem: u64,
    pub disk_bs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_bs: Option<u64>,
    pub cmdline: String,
    pub is_app: bool,
    pub is_self: bool,
}

struct Prev {
    ticks: u64,
    io_rw: Option<u64>,
    ts: u64,
}

pub struct ProcSampler {
    prev: HashMap<u32, Prev>,
    cmd_cache: HashMap<u32, String>,
    user_cache: HashMap<i64, String>,
    window_pids: HashSet<u32>,
    window_probe: Option<bool>, // None = not probed yet; Some(false) = no wm tool
    window_probe_tried: u64,
}

impl ProcSampler {
    pub fn new() -> Self {
        Self {
            prev: HashMap::new(),
            cmd_cache: HashMap::new(),
            user_cache: HashMap::new(),
            window_pids: HashSet::new(),
            window_probe: None,
            window_probe_tried: 0,
        }
    }

    /// Best-effort detection of processes that own visible windows (wmctrl).
    fn refresh_window_pids(&mut self) {
        let now = now_ms();
        if self.window_probe == Some(false) {
            return;
        }
        if now.saturating_sub(self.window_probe_tried) < 5000 {
            return;
        }
        self.window_probe_tried = now;
        if self.window_probe.is_none() {
            self.window_probe = Some(std::path::Path::new("/usr/bin/wmctrl").exists());
            if !self.window_probe.unwrap() {
                return;
            }
        }
        if let Some(out) = run("wmctrl", &["-lp"], 1500) {
            let mut s = HashSet::new();
            for line in out.lines() {
                if let Some(p) = line.trim().split_whitespace().nth(2).and_then(|p| p.parse::<u32>().ok()) {
                    s.insert(p);
                }
            }
            self.window_pids = s;
        }
    }

    fn username(&mut self, uid: i64) -> &str {
        if !self.user_cache.contains_key(&uid) {
            let name = run("getent", &["passwd", &uid.to_string()], 800)
                .and_then(|out| out.split(':').next().map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("uid {uid}"));
            self.user_cache.insert(uid, name);
        }
        self.user_cache.get(&uid).unwrap()
    }

    /// Parse /proc/<pid>/stat; comm may contain spaces/parens.
    fn parse_stat(raw: &str) -> Option<(String, char, u64, u64, u64)> {
        // (comm, state, utime, stime, vsize)
        let open = raw.find('(')?;
        let close = raw.rfind(')')?;
        if open >= close {
            return None;
        }
        let comm = raw[open + 1..close].to_string();
        let rest: Vec<&str> = raw[close + 2..].split_whitespace().collect();
        if rest.len() < 21 {
            return None;
        }
        let state = rest[0].chars().next()?;
        let utime = rest[11].parse().ok()?;
        let stime = rest[12].parse().ok()?;
        let vsize = rest[20].parse::<u64>().unwrap_or(0);
        Some((comm, state, utime, stime, vsize))
    }

    fn read_pid(&mut self, pid: u32, total_ticks: u64, now: u64) -> Option<Proc> {
        let base = format!("/proc/{pid}");
        let stat = read_text(&format!("{base}/stat"))?;
        let (comm, state, utime, stime, vsize) = Self::parse_stat(&stat)?;
        if vsize == 0 {
            return None; // kernel thread
        }

        // status + (maybe) cmdline in parallel threads
        let base1 = base.clone();
        let status_rx = std::thread::spawn(move || read_text(&format!("{base1}/status")));
        let cached_cmd = self.cmd_cache.get(&pid).cloned();
        let cmdline_rx = if cached_cmd.is_none() {
            let base2 = base.clone();
            Some(std::thread::spawn(move || read_text(&format!("{base2}/cmdline"))))
        } else {
            None
        };
        let status = status_rx.join().ok().flatten().unwrap_or_default();
        let mut uid: Option<i64> = None;
        let mut rss: u64 = 0;
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("Uid:") {
                uid = v.split_whitespace().next().and_then(|s| s.parse::<i64>().ok());
            } else if let Some(v) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = v.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) {
                    rss = kb * 1024;
                }
            }
        }
        let mut cmdline = cached_cmd.unwrap_or_default();
        if cmdline.is_empty() {
            if let Some(rx) = cmdline_rx {
                cmdline = rx.join().ok().flatten().unwrap_or_default();
            }
            if !cmdline.is_empty() {
                self.cmd_cache.insert(pid, cmdline.clone());
            }
        }
        let cmdline = cmdline.split('\0').filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");

        // disk I/O — own-uid only (others give EPERM)
        let mut io_rw = None;
        if uid == Some(self_uid()) {
            if let Some(io) = read_text(&format!("{base}/io")) {
                let rchar = io.lines().find_map(|l| l.strip_prefix("rchar:")).and_then(|s| s.trim().parse::<u64>().ok());
                let wchar = io.lines().find_map(|l| l.strip_prefix("wchar:")).and_then(|s| s.trim().parse::<u64>().ok());
                if let (Some(r), Some(w)) = (rchar, wchar) {
                    io_rw = Some(r + w);
                }
            }
        }

        let ticks = utime + stime;
        let (cpu, disk_bs) = if let Some(p) = self.prev.get(&pid) {
            let dticks = ticks.saturating_sub(p.ticks);
            let cpu = clamp(100.0 * dticks as f64 / total_ticks as f64, 0.0, 100.0);
            let disk = if now.saturating_sub(p.ts) >= 300 {
                p.io_rw.and_then(|p_io| io_rw.map(|c| (c as i128 - p_io as i128).max(0) as u64))
            } else {
                None
            };
            (cpu, disk)
        } else {
            (0.0, None)
        };
        let cpu = if cpu >= 10.0 { (cpu.round() * 10.0) / 10.0 } else { (cpu * 10.0).round() / 10.0 };

        self.prev.insert(pid, Prev { ticks, io_rw, ts: now });
        Some(Proc {
            pid,
            name: comm,
            state,
            uid: uid.unwrap_or(-1),
            user: String::new(), // filled after the loop (batched)
            cpu,
            mem: rss,
            disk_bs,
            net_bs: None,
            cmdline,
            is_app: self.window_pids.contains(&pid),
            is_self: pid == std::process::id(),
        })
    }

    pub fn sample(&mut self) -> (Vec<Proc>, i64) {
        self.refresh_window_pids();
        let pids: Vec<u32> = std::fs::read_dir("/proc")
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                    .filter_map(|n| n.parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default();

        let total_ticks: u64 = read_text("/proc/stat")
            .and_then(|t| {
                t.lines().next().map(|l| {
                    l.split_whitespace().skip(1).filter_map(|s| s.parse::<u64>().ok()).sum::<u64>()
                })
            })
            .unwrap_or(1)
            .max(1);

        let now = now_ms();
        let mut out: Vec<Proc> = Vec::new();
        let mut seen = HashSet::new();
        for &pid in &pids {
            if let Some(p) = self.read_pid(pid, total_ticks, now) {
                seen.insert(pid);
                out.push(p);
            }
        }

        // batched username resolution (one getent per distinct uid, cached)
        let uids: HashSet<i64> = out.iter().map(|p| p.uid).collect();
        for uid in uids {
            self.username(uid);
        }
        for p in out.iter_mut() {
            p.user = self.user_cache.get(&p.uid).cloned().unwrap_or_else(|| p.uid.to_string());
        }
        // drop dead pids from both caches
        self.prev.retain(|pid, _| seen.contains(pid));
        self.cmd_cache.retain(|pid, _| seen.contains(pid));

        (out, self_uid())
    }
}

pub fn kill_process(pid: u32, force: bool) -> serde_json::Value {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    let rc = unsafe { libc::kill(pid as i32, sig) };
    if rc == 0 {
        serde_json::json!({ "ok": true })
    } else {
        let err = match std::io::Error::last_os_error().raw_os_error() {
            Some(1) => "EPERM".to_string(),
            Some(3) => "ESRCH".to_string(),
            Some(e) => format!("errno {e}"),
            None => "EUNKNOWN".to_string(),
        };
        serde_json::json!({ "ok": false, "error": err })
    }
}
