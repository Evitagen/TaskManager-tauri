// Activity logger (the "Log" tab): while active, a dedicated background
// thread samples every INTERVAL_MS —
//   * per-process CPU% (jiffy deltas, % of total machine capacity)
//   * machine-wide CPU% (/proc/stat idle delta)
//   * per-process GPU usage (NVIDIA NVML compute + graphics client apps)
// Samples are kept in memory. On stop, the logger emits a review payload:
// per-app aggregation, per-app CPU timeline series (top apps), the machine
// CPU timeline, and a heuristic "suspicious activity" flag list that surfaces
// processes doing things they shouldn't (GPU use, sustained CPU, spikes,
// heavy VRAM, headless processes on the GPU).
//
// CPU semantics: a value of X means the app consumed X% of the whole machine's
// CPU capacity at that instant (one full core on an N-core box = 100/N %).
// This is independent of CONFIG_HZ because it is a ratio of jiffies over the
// same interval.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::collectors::gpu::gpu_process_usage;
use crate::collectors::util::{clamp, read_text, run};

const INTERVAL_MS: u64 = 1000;
const GPU_REFRESH_MS: u64 = 2000; // NVML process set changes slowly
const WIN_REFRESH_MS: u64 = 5000; // wmctrl (window) refresh
const CPU_REC_MIN: f64 = 1.0; // record a process at/above this % of the machine
const MAX_SAMPLES: usize = 7200; // cap ~2 h at 1 s
const TOP_SERIES: usize = 12; // how many apps get a timeline line
const DOWNSAMPLE: usize = 360; // max points per timeline series

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

struct TickProc {
    pid: u32,
    name: String,
    user: String,
    cpu: f64,
    gpu: bool,
    vram: u64,
    is_app: bool,
}

struct Tick {
    ts: u64,
    total_cpu: f64,
    procs: Vec<TickProc>,
}

/// Best-effort set of pids owning visible windows (wmctrl). Empty when the
/// tool is absent — the UI then can't label processes as "apps".
fn window_pids() -> HashSet<u32> {
    let out = match run("wmctrl", &["-lp"], 1200) {
        Some(o) => o,
        None => return HashSet::new(),
    };
    let mut s = HashSet::new();
    for line in out.lines() {
        if let Some(p) = line.trim().split_whitespace().nth(2).and_then(|p| p.parse::<u32>().ok()) {
            s.insert(p);
        }
    }
    s
}

/// Aggregate "cpu  ..." line of /proc/stat -> (total, idle+iowait).
fn aggregate_ticks(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu"))?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|s| s.parse().ok()).collect();
    if v.len() < 5 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some((total, idle))
}

/// (comm, utime, stime) from /proc/<pid>/stat.
fn parse_pid_stat(raw: &str) -> Option<(String, u64, u64)> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if open >= close {
        return None;
    }
    let rest = raw[close + 1..].split_whitespace().collect::<Vec<_>>();
    if rest.len() < 14 {
        return None;
    }
    let utime = rest[11].parse().ok()?;
    let stime = rest[12].parse().ok()?;
    let name = raw[open + 1..close].to_string();
    Some((name, utime, stime))
}

fn uid_of(pid: u32) -> Option<i64> {
    let status = read_text(&format!("/proc/{pid}/status"))?;
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("Uid:") {
            return v.split_whitespace().next().and_then(|s| s.parse().ok());
        }
    }
    None
}

pub struct ProcLogger {
    pub active: bool,
    pub start_ts: u64,
    pub interval_ms: u64,
    ticks: Vec<Tick>,
    prev_total: Option<(u64, u64)>,
    prev_proc: HashMap<u32, u64>,
    user_cache: HashMap<i64, String>,
    gpu: Vec<crate::collectors::gpu::GpuProc>,
    last_gpu: u64,
    window_pids: HashSet<u32>,
    last_win: u64,
    gpu_seen_any: bool,
}

impl ProcLogger {
    pub fn new() -> Self {
        Self {
            active: false,
            start_ts: 0,
            interval_ms: INTERVAL_MS,
            ticks: Vec::new(),
            prev_total: None,
            prev_proc: HashMap::new(),
            user_cache: HashMap::new(),
            gpu: Vec::new(),
            last_gpu: 0,
            window_pids: HashSet::new(),
            last_win: 0,
            gpu_seen_any: false,
        }
    }

    fn user(&mut self, uid: i64) -> &str {
        if !self.user_cache.contains_key(&uid) {
            let name = run("getent", &["passwd", &uid.to_string()], 600)
                .and_then(|o| o.split(':').next().map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("uid {uid}"));
            self.user_cache.insert(uid, name);
        }
        self.user_cache.get(&uid).unwrap()
    }

    /// One sampling pass. Called by the worker thread while active.
    pub fn tick(&mut self) {
        let now = now_ms();

        // machine CPU% from /proc/stat deltas
        let stat = read_text("/proc/stat").unwrap_or_default();
        let (total_cpu, dt_all) = match (aggregate_ticks(&stat), self.prev_total) {
            (Some((total, idle)), Some((pt, pi))) => {
                let dta = total.saturating_sub(pt);
                let dti = idle.saturating_sub(pi);
                let tc = if dta > 0 {
                    clamp(100.0 * (dta as f64 - dti as f64) / dta as f64, 0.0, 100.0)
                } else {
                    0.0
                };
                (tc, dta.max(1))
            }
            _ => (0.0, 1),
        };
        self.prev_total = aggregate_ticks(&stat);

        // window (app) pids, throttled
        if now.saturating_sub(self.last_win) >= WIN_REFRESH_MS {
            self.last_win = now;
            self.window_pids = window_pids();
        }

        // GPU client apps, throttled (union of compute + graphics)
        if now.saturating_sub(self.last_gpu) >= GPU_REFRESH_MS {
            self.last_gpu = now;
            self.gpu = gpu_process_usage();
            if !self.gpu.is_empty() {
                self.gpu_seen_any = true;
            }
        }
        let gpu_map: HashMap<u32, u64> =
            self.gpu.iter().map(|g| (g.pid, g.vram.unwrap_or(0))).collect();

        // per-process CPU
        let pids: Vec<u32> = std::fs::read_dir("/proc")
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                    .filter_map(|n| n.parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default();

        let mut procs: Vec<TickProc> = Vec::new();
        let mut seen = HashSet::new();
        for pid in pids {
            let raw = match read_text(&format!("/proc/{pid}/stat")) {
                Some(r) => r,
                None => continue,
            };
            let (name, utime, stime) = match parse_pid_stat(&raw) {
                Some(t) => t,
                None => continue,
            };
            let ticks = utime + stime;
            let cpu = match self.prev_proc.get(&pid) {
                Some(&pt) => {
                    let dticks = ticks.saturating_sub(pt);
                    clamp(100.0 * dticks as f64 / dt_all as f64, 0.0, 100.0)
                }
                None => 0.0,
            };
            self.prev_proc.insert(pid, ticks);
            seen.insert(pid);

            let gpu_vram = gpu_map.get(&pid);
            let is_gpu = gpu_vram.is_some();
            if cpu < CPU_REC_MIN && !is_gpu {
                continue; // not worth recording this tick
            }
            let vram = gpu_vram.copied().unwrap_or(0);
            let uid = uid_of(pid).unwrap_or(-1);
            procs.push(TickProc {
                pid,
                name,
                user: self.user(uid).to_string(),
                cpu,
                gpu: is_gpu,
                vram,
                is_app: self.window_pids.contains(&pid),
            });
        }
        self.prev_proc.retain(|pid, _| seen.contains(pid));

        self.ticks.push(Tick { ts: now, total_cpu, procs });
        if self.ticks.len() > MAX_SAMPLES {
            let drop = self.ticks.len() - MAX_SAMPLES;
            self.ticks.drain(0..drop);
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn elapsed_ms(&self) -> u64 {
        if !self.active || self.start_ts == 0 {
            return 0;
        }
        now_ms().saturating_sub(self.start_ts)
    }

    pub fn n_ticks(&self) -> usize {
        self.ticks.len()
    }

    /// Build the review payload from the collected ticks.
    pub fn review(&self) -> Value {
        let n = self.ticks.len();
        let empty = json!({ "empty": true });
        if n < 1 {
            return empty;
        }
        let start = self.ticks[0].ts;
        let end = self.ticks[n - 1].ts;
        let duration_ms = end.saturating_sub(start);
        let cores = std::thread::available_parallelism().map(|c| c.get()).unwrap_or(1);

        // ── per-app aggregation ─────────────────────────────────────────
        // An "app" is a process name (comm). Several pids can share a name
        // (helper processes, repeated launches), so at each tick we sum the
        // name's cpu across its pids first — metrics describe the whole app:
        //   active_ticks  = ticks where at least one of its pids was active
        //   peak          = the app's highest single-tick (all pids summed)
        //   sum_cpu/core_sec = integrated over the period
        struct AppAgg {
            user: String,
            is_app: bool,
            pids: Vec<u32>,
            active_ticks: u64,
            sum_cpu: f64,
            peak: f64,
            core_sec: f64,
            gpu_ticks: u64,
            max_vram: u64,
        }
        let dt_sec = self.interval_ms as f64 / 1000.0;
        let mut apps: HashMap<String, AppAgg> = HashMap::new();
        for tk in &self.ticks {
            let mut tick_cpu: HashMap<&str, f64> = HashMap::new();
            let mut tick_vram: HashMap<&str, u64> = HashMap::new();
            let mut tick_is_app: HashSet<&str> = HashSet::new();
            let mut tick_pids: HashMap<&str, Vec<u32>> = HashMap::new();
            let mut tick_user: HashMap<&str, String> = HashMap::new();
            for p in &tk.procs {
                *tick_cpu.entry(p.name.as_str()).or_insert(0.0) += p.cpu;
                if p.gpu {
                    let v = tick_vram.entry(p.name.as_str()).or_insert(0);
                    *v = (*v).max(p.vram);
                }
                if p.is_app {
                    tick_is_app.insert(p.name.as_str());
                }
                tick_pids.entry(p.name.as_str()).or_default().push(p.pid);
                tick_user.entry(p.name.as_str()).or_insert_with(|| p.user.clone());
            }
            for (name, cpu) in &tick_cpu {
                let has_gpu = tick_vram.contains_key(name);
                let vram = tick_vram.get(name).copied().unwrap_or(0);
                let a = apps.entry(name.to_string()).or_insert_with(|| AppAgg {
                    user: String::new(),
                    is_app: false,
                    pids: Vec::new(),
                    active_ticks: 0,
                    sum_cpu: 0.0,
                    peak: 0.0,
                    core_sec: 0.0,
                    gpu_ticks: 0,
                    max_vram: 0,
                });
                a.is_app = a.is_app || tick_is_app.contains(name);
                a.active_ticks += 1;
                a.sum_cpu += cpu;
                a.peak = a.peak.max(*cpu);
                a.core_sec += cpu * dt_sec / 100.0;
                if has_gpu {
                    a.gpu_ticks += 1;
                    a.max_vram = a.max_vram.max(vram);
                }
                if a.user.is_empty() {
                    if let Some(u) = tick_user.get(name) {
                        a.user = u.clone();
                    }
                }
                if let Some(ps) = tick_pids.get(name) {
                    for pid in ps {
                        if !a.pids.contains(pid) {
                            a.pids.push(*pid);
                        }
                    }
                }
            }
        }

        // app-level CPU at a tick = sum of its pids' cpu (share of machine)
        let n_ticks = n as f64;
        let mut app_rows: Vec<(String, &AppAgg)> = apps.iter().map(|(k, v)| (k.clone(), v)).collect();
        app_rows.sort_by(|a, b| b.1.core_sec.partial_cmp(&a.1.core_sec).unwrap_or(std::cmp::Ordering::Equal));

        // ── top apps get a timeline series ──────────────────────────────
        let top: HashSet<String> = app_rows.iter().take(TOP_SERIES).map(|(k, _)| k.clone()).collect();
        let mut series_map: HashMap<String, Vec<(u64, f64)>> = HashMap::new();
        if !top.is_empty() {
            for (i, tk) in self.ticks.iter().enumerate() {
                let mut per_name: HashMap<&str, f64> = HashMap::new();
                for p in &tk.procs {
                    if top.contains(p.name.as_str()) {
                        *per_name.entry(p.name.as_str()).or_insert(0.0) += p.cpu;
                    }
                }
                for (name, v) in per_name {
                    series_map.entry(name.to_string()).or_default().push((i as u64, v));
                }
            }
            // downsample long series to <= DOWNSAMPLE points
            for s in series_map.values_mut() {
                if s.len() > DOWNSAMPLE {
                    let step = s.len() / DOWNSAMPLE;
                    let mut out = Vec::with_capacity(DOWNSAMPLE);
                    let mut i = 0;
                    while i < s.len() {
                        out.push(s[i]);
                        i += step.max(1);
                    }
                    if out.last().map(|p| p.0).unwrap_or(0) != s.last().unwrap().0 {
                        out.push(*s.last().unwrap());
                    }
                    *s = out;
                }
            }
        }

        // ── suspicious-activity heuristics ──────────────────────────────
        const GPU_EXPECTED: &[&str] = &[
            "chrome", "chromium", "brave", "firefox", "vivaldi", "msedge", "edge",
            "code", "cursor", "steam", "obs", "obsidian", "blender", "kdenlive",
            "davinci", "zoom", "teams", "discord", "slack", "spotify", "vlc",
            "mpv", "xorg", "xwayland", "gnome-shell", "kwin", "plasmashell",
            "wayfire", "sway", "hyprland", "mutter", "nvidia-smi",
        ];
        let is_expected_gpu = |name: &str| {
            let lower = name.to_lowercase();
            GPU_EXPECTED.iter().any(|e| lower.contains(e))
        };
        let mut flags: Vec<Value> = Vec::new();
        for (name, a) in &app_rows {
            let avg_cpu = a.sum_cpu / n_ticks.max(1.0);
            let active_pct = 100.0 * a.active_ticks as f64 / n_ticks.max(1.0);
            let gpu_active_pct = 100.0 * a.gpu_ticks as f64 / n_ticks.max(1.0);
            let mut reasons: Vec<String> = Vec::new();
            let mut sev = 0i32;

            // GPU usage
            if a.gpu_ticks > 0 {
                let vram_gb = a.max_vram as f64 / 1e9;
                if a.is_app || is_expected_gpu(name) {
                    reasons.push(format!("used the GPU {gpu_active_pct:.0}% of the time (peak {vram_gb:.2} GB VRAM)"));
                    sev = sev.max(1);
                } else {
                    reasons.push(format!(
                        "background process on the GPU {gpu_active_pct:.0}% of the time (peak {vram_gb:.2} GB VRAM)"
                    ));
                    sev = sev.max(2);
                }
                if vram_gb >= 1.0 {
                    reasons.push(format!("holding {vram_gb:.1} GB of VRAM"));
                    sev = sev.max(1);
                }
            }
            // sustained CPU
            if avg_cpu >= 15.0 {
                reasons.push(format!("sustained {avg_cpu:.0}% of total CPU (avg over whole period)"));
                sev = sev.max(2);
            } else if avg_cpu >= 5.0 {
                reasons.push(format!("sustained {avg_cpu:.0}% of total CPU (avg over whole period)"));
                sev = sev.max(1);
            }
            // CPU spike
            if a.peak >= 90.0 {
                reasons.push(format!("spiked to {:.0}% of total CPU", a.peak));
                sev = sev.max(2);
            } else if a.peak >= 50.0 {
                reasons.push(format!("spiked to {:.0}% of total CPU", a.peak));
                sev = sev.max(1);
            }
            // background (no window) with meaningful CPU
            if !a.is_app && avg_cpu >= 3.0 && a.gpu_ticks == 0 {
                reasons.push(format!(
                    "no open window, yet {avg_cpu:.0}% of total CPU for {active_pct:.0}% of the period"
                ));
                sev = sev.max(1);
            }

            if !reasons.is_empty() {
                flags.push(json!({
                    "name": name,
                    "user": a.user,
                    "isApp": a.is_app,
                    "avgCpu": (avg_cpu * 10.0).round() / 10.0,
                    "peakCpu": (a.peak * 10.0).round() / 10.0,
                    "activePct": (active_pct * 10.0).round() / 10.0,
                    "gpuPct": (gpu_active_pct * 10.0).round() / 10.0,
                    "maxVramGb": (a.max_vram as f64 / 1e9 * 100.0).round() / 100.0,
                    "coreSec": (a.core_sec * 10.0).round() / 10.0,
                    "severity": sev,
                    "reasons": reasons,
                }));
            }
        }
        flags.sort_by(|a, b| {
            b["severity"].as_i64().unwrap_or(0)
                .cmp(&a["severity"].as_i64().unwrap_or(0))
                .then_with(|| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")))
        });

        // ── machine CPU timeline (downsampled) ──────────────────────────
        let mut total_series: Vec<Value> = Vec::new();
        if n > DOWNSAMPLE {
            let step = n / DOWNSAMPLE;
            let mut i = 0;
            while i < n {
                total_series.push(json!({ "t": self.ticks[i].ts - start, "v": self.ticks[i].total_cpu }));
                i += step.max(1);
            }
            if total_series.last().and_then(|v| v["t"].as_u64()) != Some(duration_ms) {
                total_series.push(json!({ "t": duration_ms, "v": self.ticks[n - 1].total_cpu }));
            }
        } else {
            for tk in &self.ticks {
                total_series.push(json!({ "t": tk.ts - start, "v": tk.total_cpu }));
            }
        }

        // ── per-app summary rows ────────────────────────────────────────
        let mut apps_out: Vec<Value> = Vec::new();
        for (name, a) in &app_rows {
            let avg_cpu = a.sum_cpu / n_ticks.max(1.0);
            apps_out.push(json!({
                "name": name,
                "user": a.user,
                "isApp": a.is_app,
                "pids": a.pids,
                "avgCpu": (avg_cpu * 10.0).round() / 10.0,
                "peakCpu": (a.peak * 10.0).round() / 10.0,
                "activePct": (100.0 * a.active_ticks as f64 / n_ticks.max(1.0) * 10.0).round() / 10.0,
                "coreSec": (a.core_sec * 10.0).round() / 10.0,
                "gpuPct": (100.0 * a.gpu_ticks as f64 / n_ticks.max(1.0) * 10.0).round() / 10.0,
                "maxVramGb": (a.max_vram as f64 / 1e9 * 100.0).round() / 100.0,
            }));
        }

        // ── top-app timeline series (with stable palette) ───────────────
        const PALETTE: &[&str] = &[
            "#4cc2ff", "#5ce08a", "#b08cff", "#ffc860", "#ff8f8f", "#63d3c8",
            "#e0a1ff", "#ffd479", "#7fb2ff", "#8affc1", "#ff9e9e", "#c8b6ff",
        ];
        let mut series_out: Vec<Value> = Vec::new();
        for (i, (name, _)) in app_rows.iter().take(TOP_SERIES).enumerate() {
            let pts = series_map.get(name).cloned().unwrap_or_default();
            let color = PALETTE[i % PALETTE.len()];
            series_out.push(json!({
                "name": name,
                "color": color,
                "points": pts.iter().map(|(ix, v)| json!({ "i": ix, "v": v })).collect::<Vec<_>>(),
            }));
        }

        json!({
            "empty": false,
            "startedAt": start,
            "endedAt": end,
            "durationMs": duration_ms,
            "intervalMs": self.interval_ms,
            "nTicks": n,
            "cpuCores": cores,
            "gpuProcsSupported": self.gpu_seen_any,
            "totalSeries": total_series,
            "series": series_out,
            "apps": apps_out,
            "flags": flags,
        })
    }
}

impl Default for ProcLogger {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the worker thread that ticks the logger until stopped.
pub fn start_worker(logger: std::sync::Arc<std::sync::Mutex<ProcLogger>>, interval_ms: u64) {
    std::thread::spawn(move || {
        // first tick establishes the prev baseline
        loop {
            {
                let mut lg = logger.lock().unwrap_or_else(|e| e.into_inner());
                if !lg.active {
                    return;
                }
                lg.tick();
            }
            std::thread::sleep(Duration::from_millis(interval_ms));
        }
    });
}
