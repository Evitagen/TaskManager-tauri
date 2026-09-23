// Network sampler — mirrors lib/linux.js NetSampler
// (/sys/class/net statistics deltas, physical interfaces only).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::Serialize;

use crate::collectors::util::{read_num, read_text};

static VIRTUAL_NET: &str = r"^(lo|veth.*|docker.*|br-.*|virbr.*|tap.*|tun.*|wg.*|tailscale.*|zt.*|vmnet.*|lxc.*|cali.*|flannel.*|kube.*|dummy.*|gre.*|sit.*)$";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Net {
    pub id: String,
    pub name: String,
    pub rx_bs: u64,
    pub tx_bs: u64,
    pub speed_mbps: Option<i64>,
    pub operstate: String,
    pub mac: Option<String>,
    pub ip: Option<String>,
}

pub struct NetSampler {
    re: Regex,
    prev: HashMap<String, (u64, u64, u64)>, // name -> (rx, tx, ts)
}

/// First IPv4 address of an interface via getifaddrs.
fn ipv4_of(iface: &str) -> Option<String> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return None;
    }
    let mut result = None;
    let mut p = head;
    while !p.is_null() {
        let ifa = unsafe { &*p };
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
        if name == iface && !ifa.ifa_addr.is_null() {
            let family = unsafe { (*ifa.ifa_addr).sa_family };
            if family as i32 == libc::AF_INET {
                let sa = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                let a = sa.sin_addr.s_addr; // network byte order
                result = Some(format!(
                    "{}.{}.{}.{}",
                    a as u8,
                    (a >> 8) as u8,
                    (a >> 16) as u8,
                    (a >> 24) as u8
                ));
                break;
            }
        }
        p = ifa.ifa_next;
    }
    unsafe { libc::freeifaddrs(head) };
    result
}

impl NetSampler {
    pub fn new() -> Self {
        Self {
            re: Regex::new(VIRTUAL_NET).unwrap(),
            prev: HashMap::new(),
        }
    }

    pub fn sample(&mut self) -> Vec<Net> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let ifaces = match std::fs::read_dir("/sys/class/net") {
            Ok(rd) => rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect::<Vec<_>>(),
            Err(_) => return Vec::new(),
        };

        let mut results = Vec::new();
        let mut cur: HashMap<String, (u64, u64, u64)> = HashMap::new();
        for name in ifaces {
            if self.re.is_match(&name) {
                continue;
            }
            if !std::path::Path::new(&format!("/sys/class/net/{name}/device")).exists() {
                continue; // physical only (has a PCI device), like the JS version
            }
            let rx = read_num(&format!("/sys/class/net/{name}/statistics/rx_bytes")).unwrap_or(0.0) as u64;
            let tx = read_num(&format!("/sys/class/net/{name}/statistics/tx_bytes")).unwrap_or(0.0) as u64;
            cur.insert(name.clone(), (rx, tx, now));

            let (rx_bs, tx_bs) = match self.prev.get(&name) {
                Some((prx, ptx, pts)) => {
                    let dt = ((now.saturating_sub(*pts)) as f64 / 1000.0).max(0.2);
                    (
                        ((rx.saturating_sub(*prx)) as f64 / dt).max(0.0) as u64,
                        ((tx.saturating_sub(*ptx)) as f64 / dt).max(0.0) as u64,
                    )
                }
                None => (0, 0),
            };
            let speed = read_num(&format!("/sys/class/net/{name}/speed"))
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|v| v as i64);
            let operstate = read_text(&format!("/sys/class/net/{name}/operstate"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".into());
            let mac = read_text(&format!("/sys/class/net/{name}/address"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            results.push(Net {
                id: name.clone(),
                name,
                rx_bs,
                tx_bs,
                speed_mbps: speed,
                operstate,
                mac,
                ip: None, // filled after the loop (getifaddrs once per iface)
            });
        }
        for r in results.iter_mut() {
            r.ip = ipv4_of(&r.name);
        }
        results.sort_by(|a, b| {
            let au = (a.operstate == "up") as u8;
            let bu = (b.operstate == "up") as u8;
            bu.cmp(&au).then_with(|| a.name.cmp(&b.name))
        });
        self.prev = cur;
        results
    }
}
