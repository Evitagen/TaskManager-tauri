// Multi-GPU collector:
//   1. nvidia-smi (NVML) with 1 s backoff + last-good cache
//   2. /sys/class/drm (amdgpu busy %, i915/xe gt busy, hwmon temp/power)
//   3. /proc/driver/nvidia + lspci enumeration (no telemetry)

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::collectors::util::{clamp, read_num, read_text, run};

const NV_SMI_QUERY: &str = "index,name,driver_version,utilization.gpu,utilization.memory,memory.used,memory.total,temperature.gpu,power.draw,power.limit,fan.speed,clocks.gr,clocks.mem,clocks.max.sm,uuid";

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn num_field(s: &str) -> Option<f64> {
    if s == "[N/A]" || s == "N/A" || s.is_empty() {
        return None;
    }
    s.parse().ok()
}

pub struct GpuCollector {
    lspci_map: Option<HashMap<String, String>>,
    last_nvml_attempt: u64,
    last_nvml: Option<Vec<Value>>,
    lspci_re: Regex,
}

impl GpuCollector {
    pub fn new() -> Self {
        Self {
            lspci_map: None,
            last_nvml_attempt: 0,
            last_nvml: None,
            lspci_re: Regex::new(r"^([0-9a-fA-F:.]+)\s+.*?\b(?:VGA compatible controller|3D controller|Display controller):\s+(.*)$").unwrap(),
        }
    }

    /// "0000:65:00.0" -> "NVIDIA Corporation GA102 [GeForce RTX 3090]"
    fn lspci_names(&mut self) -> &HashMap<String, String> {
        if self.lspci_map.is_none() {
            let mut map = HashMap::new();
            if let Some(out) = run("lspci", &["-D"], 3000) {
                for line in out.lines() {
                    if let Some(c) = self.lspci_re.captures(line) {
                        let model = c.get(2).map(|m| m.as_str()).unwrap_or("")
                            .split("(rev ").map(|s| s.trim()).collect::<Vec<_>>()[0].trim().to_string();
                        map.insert(c.get(1).unwrap().as_str().to_lowercase(), model);
                    }
                }
            }
            self.lspci_map = Some(map);
        }
        self.lspci_map.as_ref().unwrap()
    }

    fn from_nvidia_smi(&mut self) -> Option<Vec<Value>> {
        let now = now_ms();
        if now.saturating_sub(self.last_nvml_attempt) < 1000 {
            return self.last_nvml.clone();
        }
        self.last_nvml_attempt = now;
        let query = format!("--query-gpu={NV_SMI_QUERY}");
        let out = run("nvidia-smi", &[&query, "--format=csv,noheader,nounits"], 2500);
        if let Some(t) = std::env::var("TM_GPU_DEBUG").ok() {
            if !t.is_empty() {
                eprintln!("[gpu-debug] nvml raw: {:?}", out.as_ref().map(|s| s.chars().take(300).collect::<String>()));
            }
        }
        let Some(out) = out else { return self.last_nvml.clone() };
        if out.trim().is_empty() {
            return self.last_nvml.clone();
        }
        let mut gpus = Vec::new();
        for line in out.trim().lines() {
            let f: Vec<String> = line.split(',').map(|s| s.trim().to_string()).collect();
            if f.len() < 14 {
                continue;
            }
            let n = |i: usize| num_field(&f[i]);
            let mem_used = n(5).map(|v| v * 1048576.0);
            let mem_total = n(6).map(|v| v * 1048576.0);
            gpus.push(json!({
                "index": n(0),
                "model": f[1],
                "driver": f[2],
                "util": clamp(n(3).unwrap_or(0.0), 0.0, 100.0),
                "utilMem": n(4),
                "memUsed": mem_used,
                "memTotal": mem_total,
                "temp": n(7),
                "powerW": n(8),
                "powerLimitW": n(9),
                "fanPct": n(10),
                "clockGpuMHz": n(11),
                "clockMemMHz": n(12),
                "clockMaxMHz": n(13),
                "vendor": "NVIDIA",
                "source": "nvml",
                "telemetry": true,
            }));
        }
        if gpus.is_empty() {
            return self.last_nvml.clone();
        }
        self.last_nvml = Some(gpus.clone());
        Some(gpus)
    }

    fn from_drm(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        let cards = match std::fs::read_dir("/sys/class/drm") {
            Ok(rd) => rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| Regex::new(r"^card\d+$").unwrap().is_match(n))
                .collect::<Vec<_>>(),
            Err(_) => return out,
        };
        for card in cards {
            let base = format!("/sys/class/drm/{card}/device");
            if !std::path::Path::new(&base).exists() {
                continue;
            }
            let vendor_hex = read_text(&format!("{base}/vendor")).unwrap_or_default();
            let vendor_id = u32::from_str_radix(vendor_hex.trim().trim_start_matches("0x"), 16).unwrap_or(0);
            if vendor_id == 0 {
                continue;
            }
            if vendor_id == 0x10de {
                continue; // NVIDIA handled by NVML branch
            }
            let slot = read_text(&format!("{base}/uevent"))
                .and_then(|t| t.lines().find_map(|l| l.strip_prefix("PCI_SLOT_NAME=").map(|s| s.trim().to_lowercase())));

            let mut busy = read_num(&format!("{base}/gpu_busy_percent")); // amdgpu
            let mem_busy = read_num(&format!("{base}/mem_busy_percent"));
            let used_mib = read_num(&format!("{base}/mem_info_vram_used"));
            let total_mib = read_num(&format!("{base}/mem_info_vram_total"));
            let mem_used = used_mib.map(|v| v * 1048576.0);
            let mem_total = total_mib.map(|v| v * 1048576.0);
            if busy.is_none() {
                // i915 / xe: average gt*/gt_busy_percent
                if let Ok(rd) = std::fs::read_dir(format!("/sys/class/drm/{card}")) {
                    let vals: Vec<f64> = rd.filter_map(|e| e.ok())
                        .filter_map(|e| e.file_name().into_string().ok())
                        .filter(|n| {
                            let gt = n.strip_suffix("").unwrap_or(n);
                            gt.starts_with("gt") && gt[2..].chars().all(|c| c.is_ascii_digit() || c == '-')
                                && !gt.eq("gt")
                        })
                        .filter_map(|n| read_num(&format!("/sys/class/drm/{card}/{n}/gt_busy_percent")))
                        .collect();
                    if !vals.is_empty() {
                        busy = Some(vals.iter().sum::<f64>() / vals.len() as f64);
                    }
                }
            }
            let mut temp = None;
            if let Ok(hwmons) = std::fs::read_dir(&format!("{base}/hwmon")) {
                for h in hwmons.filter_map(|e| e.ok()) {
                    for tname in ["temp1_input", "temp2_input"] {
                        if let Some(t) = read_num(&format!("{base}/hwmon/{}/{}", h.file_name().to_string_lossy(), tname)) {
                            temp = Some((t / 1000.0).round());
                            break;
                        }
                    }
                    if temp.is_some() {
                        break;
                    }
                }
            }
            let power_w = read_num_glob(&format!("{base}/hwmon/hwmon*/power1_input")).map(|v| (v / 1e6).round() / 10.0);

            let lspci = self.lspci_names();
            let model = slot.as_ref().and_then(|s| lspci.get(s)).cloned().unwrap_or_else(|| card.clone());
            let vendor = match vendor_id {
                0x1002 => "AMD",
                0x8086 => "Intel",
                0x10de => "NVIDIA",
                _ => "Other",
            };
            let telemetry = busy.is_some() || mem_used.is_some();
            out.push(json!({
                "slot": slot,
                "model": model,
                "vendor": vendor,
                "source": "sysfs",
                "util": busy.map(|v| clamp(v.round(), 0.0, 100.0)),
                "utilMem": mem_busy.map(|v| v.round()),
                "memUsed": mem_used,
                "memTotal": mem_total,
                "temp": temp,
                "powerW": power_w,
                "telemetry": telemetry,
            }));
        }
        out
    }

    /// NVIDIA GPUs present but with no working NVML.
    fn from_proc_driver(&mut self) -> (Vec<Value>, Option<String>) {
        let driver = read_text("/proc/driver/nvidia/version").and_then(|t| {
            Regex::new(r"Kernel Module\s+([\d.]+)")
                .ok()
                .and_then(|re| re.captures(&t).map(|c| c[1].to_string()))
        });
        let mut gpus = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/proc/driver/nvidia/gpus") {
            for entry in rd.filter_map(|e| e.ok()) {
                let bus = entry.file_name().to_string_lossy().into_owned();
                let Some(info) = read_text(&format!("/proc/driver/nvidia/gpus/{bus}/information")) else { continue };
                let model = Regex::new(r"Model\s*:\s*(.+)$").ok()
                    .and_then(|re| info.lines().find_map(|l| re.captures(l)).map(|c| c[1].trim().to_string()));
                let bus_loc = Regex::new(r"Bus Location\s*:\s*(.+)$").ok()
                    .and_then(|re| info.lines().find_map(|l| re.captures(l)).map(|c| c[1].trim().to_lowercase()));
                if model.is_none() && bus_loc.as_ref().is_some_and(|b| self.lspci_names().contains_key(b)) {
                    continue; // avoid dupes with lspci-only entries
                }
                let lspci = self.lspci_names();
                let model = model.or_else(|| bus_loc.as_ref().and_then(|b| lspci.get(b).cloned()))
                    .unwrap_or_else(|| format!("NVIDIA GPU ({bus})"));
                gpus.push(json!({
                    "slot": bus_loc,
                    "model": model,
                    "vendor": "NVIDIA",
                    "driver": driver.clone(),
                    "source": "proc",
                    "telemetry": false,
                }));
            }
        }
        // lspci-only GPUs so multi-GPU always enumerates
        if gpus.is_empty() {
            for (slot, model) in self.lspci_names() {
                let vendor = if model.to_lowercase().contains("nvidia") { "NVIDIA" }
                    else if model.to_lowercase().contains("amd") || model.to_lowercase().contains("ati") { "AMD" }
                    else if model.to_lowercase().contains("intel") { "Intel" }
                    else { "Other" };
                gpus.push(json!({
                    "slot": slot,
                    "model": model,
                    "vendor": vendor,
                    "driver": driver.clone(),
                    "source": "lspci",
                    "telemetry": false,
                }));
            }
        }
        (gpus, driver)
    }

    pub fn sample(&mut self) -> Vec<Value> {
        let mut merged: Vec<(String, Value)> = Vec::new();
        if let Some(nvml) = self.from_nvidia_smi() {
            for (i, g) in nvml.into_iter().enumerate() {
                let key = g.get("index").and_then(|v| v.as_u64()).map(|n| format!("nvml:{n}")).unwrap_or_else(|| format!("nvml:{i}"));
                merged.push((key, g));
            }
        } else {
            for g in self.from_drm() {
                let key = g.get("slot").and_then(|v| v.as_str()).or_else(|| g.get("model").and_then(|v| v.as_str()))
                    .unwrap_or("unknown").to_string();
                merged.push((format!("drm:{key}"), g));
            }
        }
        if merged.is_empty() {
            let (gpus, driver) = self.from_proc_driver();
            for g in gpus {
                let key = g.get("slot").and_then(|v| v.as_str()).or_else(|| g.get("model").and_then(|v| v.as_str()))
                    .unwrap_or("unknown").to_string();
                let mut g = g;
                if g.get("driver").and_then(|d| d.as_str()).is_none() {
                    g["driver"] = json!(driver);
                }
                merged.push((format!("pcidrv:{key}"), g));
            }
        }
        merged.into_iter().enumerate().map(|(i, (_, mut g))| {
            g["id"] = json!(format!("gpu{i}"));
            g["label"] = json!(format!("GPU {}", i + 1));
            g
        }).collect()
    }
}

/// Per-process GPU usage (NVIDIA only): the union of NVML compute and
/// graphics client apps. Empty when the driver / NVML is unavailable
/// (missing /dev/nvidia* nodes, AMD/Intel, ...) — callers degrade to a
/// CPU-only view in that case.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GpuProc {
    pub pid: u32,
    pub name: String,
    pub vram: Option<u64>, // bytes
    pub kind: String, // "compute" | "graphics"
}

fn parse_gpu_app_csv(out: Option<&str>, kind: &str) -> Vec<GpuProc> {
    let Some(out) = out.filter(|s| !s.trim().is_empty()) else { return Vec::new() };
    let mut outv = Vec::new();
    for line in out.lines() {
        let f: Vec<String> = line.split(',').map(|s| s.trim().to_string()).collect();
        if f.len() < 2 {
            continue;
        }
        let pid = match f[0].parse::<u32>() { Ok(p) => p, Err(_) => continue };
        let vram = f.get(2).and_then(|s| s.parse::<f64>().ok()).map(|mb| (mb * 1048576.0) as u64);
        outv.push(GpuProc { pid, name: f[1].clone(), vram, kind: kind.to_string() });
    }
    outv
}

pub fn gpu_process_usage() -> Vec<GpuProc> {
    let compute = run("nvidia-smi", &["--query-compute-apps=pid,process_name,used_memory", "--format=csv,noheader,nounits"], 2500);
    let graphics = run("nvidia-smi", &["--query-graphics-apps=pid,process_name,used_memory", "--format=csv,noheader,nounits"], 2500);
    let mut merged: Vec<GpuProc> = Vec::new();
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (v, kind) in [(&compute, "compute"), (&graphics, "graphics")] {
        for p in parse_gpu_app_csv(v.as_deref(), kind) {
            if seen.insert(p.pid) {
                merged.push(p);
            }
        }
    }
    merged
}

/// Read the first numeric file matching a pattern with one trailing
/// "*/segment" wildcard (like util readNumberGlob in gpu.js).
fn read_num_glob(pattern: &str) -> Option<f64> {
    let parts: Vec<&str> = pattern.split('/').collect();
    let star_i = parts.iter().position(|p| p.contains('*'));
    let Some(i) = star_i else { return read_num(pattern) };
    let base = parts[..i].join("/");
    let seg = parts[i];
    let re = Regex::new(&format!("^{}$", seg.replace('*', ".*"))).ok()?;
    let rest = parts[i + 1..].join("/");
    let entries = std::fs::read_dir(&base).ok()?;
    for e in entries.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().into_owned();
        if re.is_match(&name) {
            if let Some(v) = read_num(&format!("{base}/{name}/{rest}")) {
                return Some(v);
            }
        }
    }
    None
}
