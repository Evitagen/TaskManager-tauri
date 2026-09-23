// Memory sampler (/proc/meminfo).

use serde::Serialize;
use crate::collectors::util::read_text;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemSample {
    pub total: u64,
    pub in_use: u64,
    pub available: u64,
    pub cached: i64,
    pub buffers: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub commit_limit: Option<u64>,
    pub committed: Option<u64>,
}

pub fn sample() -> Option<MemSample> {
    let text = read_text("/proc/meminfo")?;
    let mut kv: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for line in text.lines() {
        // "MemTotal:       97894336 kB"
        if let Some((k, rest)) = line.split_once(':') {
            if let Some(num) = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) {
                kv.insert(k.trim(), num * 1024); // kB -> bytes
            }
        }
    }
    let total = kv.get("MemTotal").copied().unwrap_or(0);
    let available = kv.get("MemAvailable").or_else(|| kv.get("MemFree")).copied().unwrap_or(0);
    let in_use = total.saturating_sub(available);
    let cached: i64 = kv.get("Cached").copied().unwrap_or(0) as i64
        + kv.get("SReclaimable").copied().unwrap_or(0) as i64
        - kv.get("SwapCached").copied().unwrap_or(0) as i64;
    let swap_total = kv.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = kv.get("SwapFree").copied().unwrap_or(0);
    Some(MemSample {
        total,
        in_use,
        available,
        cached,
        buffers: kv.get("Buffers").copied().unwrap_or(0),
        swap_total,
        swap_used: swap_total.saturating_sub(swap_free),
        commit_limit: kv.get("CommitLimit").copied(),
        committed: kv.get("Committed_AS").copied(),
    })
}
