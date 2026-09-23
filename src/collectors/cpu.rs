// CPU sampler — mirrors lib/linux.js CpuSampler (/proc/stat deltas).

use serde::Serialize;
use crate::collectors::util::{clamp, read_num, read_text, round};

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CpuInfo {
    pub model: String,
    pub logical: usize,
    pub physical: usize,
    pub base_freq: Option<f64>, // MHz
    pub max_freq: Option<f64>,  // MHz
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuSample {
    pub usage: u32,
    pub per_core: Vec<u32>,
    pub load: Option<Vec<f64>>,
    pub uptime_sec: Option<u64>,
    pub freq_ghz: Option<f64>,
}

struct Prev {
    totals: u64,
    idle: u64,
    per_core: Vec<(u64, u64)>,
}

pub struct CpuSampler {
    prev: Option<Prev>,
    info: Option<CpuInfo>,
}

fn parse_cpu_line(line: &str) -> Option<(u64, u64)> {
    // line like "cpu0  123 45 678 ..." or "cpu  123 45 678 ..."
    let rest = line.splitn(2, ' ').nth(1)?;
    let v: Vec<u64> = rest.split_whitespace().filter_map(|t| t.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some((total, idle))
}

impl CpuSampler {
    pub fn new() -> Self {
        Self { prev: None, info: None }
    }

    pub fn info(&mut self) -> &CpuInfo {
        if self.info.is_none() {
            let stat = read_text("/proc/stat").unwrap_or_default();
            let cpuinfo = read_text("/proc/cpuinfo").unwrap_or_default();

            let logical = stat.lines().filter(|l| {
                let rest = l.strip_prefix("cpu").unwrap_or(l);
                rest.chars().next().is_some_and(|c| c.is_ascii_digit())
            }).count();

            let model = cpuinfo.lines().find_map(|l| {
                l.strip_prefix("model name")
                    .and_then(|r| r.splitn(2, ':').nth(1))
                    .map(|s| s.trim().to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Unknown CPU".into());

            let mut ids = std::collections::HashSet::new();
            let mut pkg = String::new();
            for line in cpuinfo.lines() {
                if let Some(v) = line.strip_prefix("physical id") {
                    pkg = v.splitn(2, ':').nth(1).unwrap_or("").trim().to_string();
                } else if let Some(v) = line.strip_prefix("core id") {
                    let core = v.splitn(2, ':').nth(1).unwrap_or("").trim();
                    ids.insert(format!("{pkg}:{core}"));
                }
            }
            let physical = if ids.is_empty() { logical } else { ids.len() };

            let base_freq = read_num("/sys/devices/system/cpu/cpu0/cpufreq/base_frequency")
                .map(|v| v / 1000.0); // kHz -> MHz
            let max_freq = read_num("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
                .map(|v| v / 1000.0)
                .or_else(|| {
                    cpuinfo.lines().find_map(|l| {
                        l.strip_prefix("cpu MHz").and_then(|r| r.splitn(2, ':').nth(1))
                    }).and_then(|s| s.trim().parse::<f64>().ok()).map(|v| v.round())
                });

            self.info = Some(CpuInfo { model, logical, physical, base_freq, max_freq });
        }
        self.info.as_ref().unwrap()
    }

    /// Average of the per-core "cpu MHz" fields, in GHz (2 dp); fallback cpu0 scaling_cur_freq.
    pub fn current_freq_ghz(&self) -> Option<f64> {
        let cpuinfo = read_text("/proc/cpuinfo");
        if let Some(text) = cpuinfo {
            let vals: Vec<f64> = text.lines().filter_map(|l| {
                l.strip_prefix("cpu MHz")
                    .and_then(|r| r.splitn(2, ':').nth(1))
                    .and_then(|s| s.trim().parse::<f64>().ok())
            }).collect();
            if !vals.is_empty() {
                return Some(round(vals.iter().sum::<f64>() / vals.len() as f64 / 1000.0, 2));
            }
        }
        read_num("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq").map(|v| round(v / 1e6, 2))
    }

    pub fn sample(&mut self) -> Option<CpuSample> {
        let text = read_text("/proc/stat")?;
        let mut per_core: Vec<(u64, u64)> = Vec::new();
        let mut totals = 0u64;
        let mut idle_all = 0u64;
        for line in text.lines() {
            if !line.starts_with("cpu") {
                continue;
            }
            let is_total = line[3..].starts_with(' ');
            let (total, idle) = match parse_cpu_line(line) {
                Some(v) => v,
                None => continue,
            };
            if is_total {
                totals = total;
                idle_all = idle;
            } else {
                per_core.push((total, idle));
            }
        }

        let (usage, usage_core) = if let Some(p) = &self.prev {
            let dt = totals.saturating_sub(p.totals).max(1);
            let usage = clamp(100.0 * (1.0 - (idle_all as f64 - p.idle as f64) / dt as f64), 0.0, 100.0);
            let usage_core = per_core.iter().enumerate().map(|(i, c)| {
                let p = match p.per_core.get(i) {
                    Some(p) => p,
                    None => return 0.0,
                };
                let t = c.0.saturating_sub(p.0);
                if t == 0 { 0.0 } else { clamp(100.0 * (1.0 - (c.1 as f64 - p.1 as f64) / t as f64), 0.0, 100.0) }
            }).collect::<Vec<f64>>();
            (usage, usage_core)
        } else {
            (0.0, vec![0.0; per_core.len()])
        };
        self.prev = Some(Prev { totals, idle: idle_all, per_core });

        let load = read_text("/proc/loadavg").and_then(|t| {
            t.split_whitespace().take(3).map(|s| s.parse::<f64>().ok()).collect::<Option<Vec<_>>>()
        });
        let uptime_sec = read_text("/proc/uptime")
            .and_then(|t| t.split_whitespace().next().map(|s| s.to_string()))
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| v as u64);

        Some(CpuSample {
            usage: usage.round() as u32,
            per_core: usage_core.iter().map(|v| v.round() as u32).collect(),
            load,
            uptime_sec,
            freq_ghz: self.current_freq_ghz(),
        })
    }
}
