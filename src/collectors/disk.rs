// Disk sampler (/proc/diskstats deltas + /sys/block meta + mount usage via statfs).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::Serialize;

use crate::collectors::util::{clamp, read_num, read_text, round};

static WHOLE_DISK: &str = r"^(sd[a-z]+|hd[a-z]+|vd[a-z]+|xvd[a-z]+|nvme\d+n\d+|mmcblk\d+)$";

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsage {
    pub mount: String,
    pub used_pct: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    #[serde(rename = "type")]
    pub disk_type: Option<String>,
    pub size: Option<u64>,
    pub usage: Option<DiskUsage>,
    pub active: f64,
    pub read_bs: u64,
    pub write_bs: u64,
    pub in_progress: u64,
}

#[derive(Clone)]
struct DeviceMeta {
    model: Option<String>,
    disk_type: Option<String>,
    size: Option<u64>,
}

pub struct DiskSampler {
    re: Regex,
    prev: Option<HashMap<String, (Vec<f64>, u64)>>, // name -> (fields, ts)
    meta_cache: HashMap<String, DeviceMeta>,
    usage_ts: u64,
    usage_map: HashMap<String, DiskUsage>,
}

/// statfs a mount point; (total bytes, used pct).
fn statfs(mount: &str) -> Option<(u64, f64)> {
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path(mount)?.as_ptr() as *const libc::c_char, &mut buf) };
    if rc != 0 {
        return None;
    }
    let bsize = buf.f_bsize as u64;
    let total = buf.f_blocks as u64 * bsize;
    if total == 0 {
        return None;
    }
    let free = buf.f_bfree as u64 * bsize;
    Some((total, 100.0 * (1.0 - free as f64 / total as f64)))
}

fn c_path(s: &str) -> Option<Vec<u8>> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    Some(v)
}

impl DiskSampler {
    pub fn new() -> Self {
        let re = Regex::new(WHOLE_DISK).unwrap();
        Self {
            re,
            prev: None,
            meta_cache: HashMap::new(),
            usage_ts: 0,
            usage_map: HashMap::new(),
        }
    }

    fn meta(&mut self, name: &str) -> &DeviceMeta {
        if !self.meta_cache.contains_key(name) {
            let mut model = read_text(&format!("/sys/block/{name}/device/model"))
                .map(|s| s.replace('\0', "").trim().to_string())
                .filter(|s| !s.is_empty());
            if model.is_none() && name.starts_with("nvme") {
                if let Some(ctrl) = name.rsplit_once('n').map(|(c, _)| c) {
                    model = read_text(&format!("/sys/class/{ctrl}/model"))
                        .map(|s| s.replace('\0', "").trim().to_string())
                        .filter(|s| !s.is_empty());
                }
            }
            let disk_type = read_text(&format!("/sys/block/{name}/queue/rotational"))
                .map(|t| if t.trim() == "0" { "SSD".to_string() } else { "HDD".to_string() })
                .or_else(|| name.starts_with("nvme").then(|| "SSD".to_string()));
            let size = read_num(&format!("/sys/block/{name}/size")).map(|v| (v as u64) * 512);
            self.meta_cache
                .insert(name.to_string(), DeviceMeta { model, disk_type, size });
        }
        self.meta_cache.get(name).unwrap()
    }

    /// used-space % of the largest mounted partition belonging to this disk (10 s cache).
    fn mount_usage(&mut self, name: &str, now: u64) -> Option<DiskUsage> {
        if self.usage_map.is_empty() || now.saturating_sub(self.usage_ts) > 10000 {
            let mut map: HashMap<String, DiskUsage> = HashMap::new();
            if let Some(mounts) = read_text("/proc/mounts") {
                // disk -> best (mount, total, used_pct)
                let mut candidates: HashMap<String, (String, u64, f64)> = HashMap::new();
                let part_re = Regex::new(r"^(sd[a-z]+|hd[a-z]+|vd[a-z]+|xvd[a-z]+|nvme\d+n\d+|mmcblk\d+)").unwrap();
                for line in mounts.lines() {
                    let f: Vec<&str> = line.split_whitespace().collect();
                    let dev = match f.first() {
                        Some(d) if d.starts_with("/dev/") => d,
                        _ => continue,
                    };
                    let base = dev.strip_prefix("/dev/").unwrap_or("");
                    let disk = match part_re.captures(base).and_then(|c| c.get(1)) {
                        Some(m) => m.as_str(),
                        None => continue,
                    };
                    if let Some(mnt) = f.get(1) {
                        if let Some((total, used_pct)) = statfs(mnt) {
                            let e = candidates.entry(disk.to_string()).or_insert_with(|| (mnt.to_string(), 0, 0.0));
                            if total > e.1 {
                                *e = (mnt.to_string(), total, used_pct);
                            }
                        }
                    }
                }
                for (disk, (mnt, _, used_pct)) in candidates {
                    map.insert(disk, DiskUsage { mount: mnt, used_pct: round(used_pct, 1) });
                }
                self.usage_map = map;
                self.usage_ts = now;
            }
        }
        self.usage_map.get(name).cloned()
    }

    pub fn sample(&mut self) -> Vec<Disk> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let text = read_text("/proc/diskstats").unwrap_or_default();
        // Keep the /proc/diskstats line order: the renderer rebuilds the grid
        // whenever the id order changes, and HashMap iteration order is
        // randomized per map, which would reshuffle the disks every tick.
        let mut cur: HashMap<String, Vec<f64>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for line in text.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 14 {
                continue;
            }
            let fields: Vec<f64> = f[3..].iter().filter_map(|t| t.parse().ok()).collect();
            if !cur.contains_key(f[2]) {
                order.push(f[2].to_string());
            }
            cur.insert(f[2].to_string(), fields);
        }

        let mut out = Vec::new();
        for name in &order {
            let f = &cur[name];
            if !self.re.is_match(name) {
                continue;
            }
            let p = self.prev.as_ref().and_then(|prev| prev.get(name));
            let (mut read_bs, mut write_bs, mut active) = (0u64, 0u64, 0f64);
            if let Some((pf, pts)) = p {
                let dt_ms = (now.saturating_sub(*pts)).max(1) as f64;
                let d_read = f.get(2).zip(pf.get(2)).map(|(a, b)| (a - b).max(0.0));
                let d_write = f.get(6).zip(pf.get(6)).map(|(a, b)| (a - b).max(0.0));
                let d_io_ms = f.get(9).zip(pf.get(9)).map(|(a, b)| (a - b).max(0.0));
                if let Some(dr) = d_read {
                    read_bs = (dr * 512.0 / dt_ms * 1000.0).max(0.0) as u64;
                }
                if let Some(dw) = d_write {
                    write_bs = (dw * 512.0 / dt_ms * 1000.0).max(0.0) as u64;
                }
                if let Some(dms) = d_io_ms {
                    active = clamp(100.0 * dms / dt_ms, 0.0, 100.0);
                }
            }
            let meta = self.meta(name).clone();
            let usage = self.mount_usage(name, now);
            if meta.size.is_some() {
                out.push(Disk {
                    id: name.clone(),
                    name: name.clone(),
                    model: meta.model.clone(),
                    disk_type: meta.disk_type.clone(),
                    size: meta.size,
                    usage,
                    active: round(active, 1),
                    read_bs: read_bs,
                    write_bs: write_bs,
                    in_progress: f.get(8).copied().unwrap_or(0.0) as u64,
                });
            }
        }
        self.prev = Some(cur.iter().map(|(k, v)| (k.clone(), (v.clone(), now))).collect());
        out
    }
}
