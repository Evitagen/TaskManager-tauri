// Ground-truth tests for the Rust collectors (headless, no webview needed).

use std::time::Duration;
use task_manager_lib::collectors::Hub;

fn sleep(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[test]
fn cpu_sample_shape_and_range() {
    let mut hub = Hub::new();
    let info = hub.cpu.info().clone();
    assert!(info.logical >= 1, "logical cores: {info:?}");
    assert!(!info.model.is_empty());
    // host is an i9-10940X: 28 logical / 14 physical
    assert_eq!(info.logical, 28, "logical should be 28 on this host: {info:?}");
    assert_eq!(info.physical, 14, "physical should be 14 on this host: {info:?}");

    let _first = hub.cpu.sample().expect("first cpu sample");
    sleep(700);
    let s = hub.cpu.sample().expect("second cpu sample");
    assert!(s.usage <= 100, "usage in range: {}", s.usage);
    assert_eq!(s.per_core.len(), info.logical, "perCore count");
    for c in &s.per_core {
        assert!(*c <= 100);
    }
    assert_eq!(s.load.as_ref().map(|l| l.len()), Some(3), "loadavg triple");
    assert!(s.uptime_sec.unwrap() > 3600, "uptime plausible");
}

#[test]
fn mem_sample_plausible() {
    let mut hub = Hub::new();
    let perf = hub.performance();
    let mem = perf["memory"].as_object().expect("memory object");
    let total = mem["total"].as_u64().unwrap();
    let in_use = mem["inUse"].as_u64().unwrap();
    // host has ~94 GB
    assert!(total > 60_000_000_000 && total < 200_000_000_000, "total bytes: {total}");
    assert!(in_use < total, "inUse < total");
    assert!(mem["available"].as_u64().unwrap() < total);
    assert!(mem["swapTotal"].as_u64().is_some());
}

#[test]
fn disk_sample_lists_whole_disks() {
    let mut hub = Hub::new();
    let _ = hub.disk.sample(); // warm meta cache
    sleep(300);
    let disks = hub.disk.sample();
    assert!(!disks.is_empty(), "at least one whole disk");
    for d in &disks {
        assert!(d.size.unwrap() > 1_000_000_000, "{} size {}", d.id, d.size.unwrap());
        assert!(matches!(d.disk_type.as_deref(), Some("SSD") | Some("HDD")));
        if let Some(u) = &d.usage {
            assert!((0.0..=100.0).contains(&u.used_pct), "{} usedPct {}", d.id, u.used_pct);
        }
    }
    // no partitions (sda1, nvme0n1p1) may leak in
    let whole = regex::Regex::new(r"^(sd[a-z]+|hd[a-z]+|vd[a-z]+|xvd[a-z]+|nvme\d+n\d+|mmcblk\d+)$").unwrap();
    for d in &disks {
        assert!(whole.is_match(&d.id), "partition leaked: {}", d.id);
    }
}

#[test]
fn disk_sample_order_is_stable() {
    // Regression: the disk grid used to reshuffle every tick because the
    // sampler iterated a freshly built HashMap (randomized order per sample).
    let mut hub = Hub::new();
    let _ = hub.disk.sample(); // warm meta cache
    sleep(300);
    let a = hub.disk.sample();
    sleep(300);
    let b = hub.disk.sample();
    let oa: Vec<&str> = a.iter().map(|d| d.id.as_str()).collect();
    let ob: Vec<&str> = b.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(oa, ob, "disk order must not change between samples: {oa:?} vs {ob:?}");

    // order must match the /proc/diskstats line order (what the renderer
    // rebuild key depends on)
    let whole = regex::Regex::new(r"^(sd[a-z]+|hd[a-z]+|vd[a-z]+|xvd[a-z]+|nvme\d+n\d+|mmcblk\d+)$").unwrap();
    let text = std::fs::read_to_string("/proc/diskstats").expect("read /proc/diskstats");
    let expected: Vec<String> = text
        .lines()
        .filter_map(|l| l.split_whitespace().nth(2).map(|s| s.to_string()))
        .filter(|s| whole.is_match(s))
        .collect();
    let got: Vec<String> = oa.iter().map(|s| s.to_string()).collect();
    assert_eq!(got, expected, "order follows /proc/diskstats: {got:?} vs {expected:?}");
}

#[test]
fn net_sample_physical_only() {
    let mut hub = Hub::new();
    sleep(300);
    let _ = hub.net.sample();
    sleep(300);
    let nets = hub.net.sample();
    assert!(!nets.is_empty(), "at least one net iface");
    for n in &nets {
        assert_ne!(n.name, "lo", "lo excluded");
        assert!(!n.name.starts_with("veth") && !n.name.starts_with("docker") && !n.name.starts_with("br-") && !n.name.starts_with("virbr"), "virtual excluded: {}", n.name);
        // speed may legitimately be null on unlinked ports (renderer shows "—")
    }
}

#[test]
fn gpu_sample_two_nvidia_3090s() {
    let mut hub = Hub::new();
    let gpus = hub.gpu.sample();
    assert_eq!(gpus.len(), 2, "two GPUs: {gpus:?}");
    for g in &gpus {
        assert_eq!(g["vendor"], "NVIDIA");
        assert!(g["model"].as_str().unwrap().contains("RTX 3090"), "model: {}", g["model"]);
    }
    // if NVML is live (dev nodes present): full telemetry assertions
    let smi_ok = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    if smi_ok {
        for g in &gpus {
            assert_eq!(g["source"], "nvml", "source nvml");
            assert_eq!(g["telemetry"], true);
            assert!((0.0..=100.0).contains(&g["util"].as_f64().unwrap()), "util range");
            let mem_total = g["memTotal"].as_f64().unwrap();
            assert!((mem_total - 24.0 * 1024.0 * 1024.0 * 1024.0).abs() < 2.0e9, "24 GiB VRAM, got {mem_total}");
            let mem_used = g["memUsed"].as_f64().unwrap();
            assert!(mem_used >= 0.0 && mem_used <= mem_total, "memUsed {mem_used}");
            assert!((10.0..=95.0).contains(&g["temp"].as_f64().unwrap()), "temp plausible: {}", g["temp"]);
        }
        // cross-check against live nvidia-smi (mem total must match exactly)
        let out = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
            .output()
            .expect("nvidia-smi runs");
        let smi: Vec<u64> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        assert_eq!(smi.len(), 2);
        for (g, mb) in gpus.iter().zip(smi.iter()) {
            assert_eq!(g["memTotal"].as_f64().unwrap(), (*mb as f64) * 1048576.0, "memTotal matches nvidia-smi");
        }
    } else {
        // NVML unreachable: fallback chain must still enumerate both GPUs (no telemetry)
        for g in &gpus {
            assert_eq!(g["telemetry"], false, "fallback enumeration: {g:?}");
            assert!(["proc", "lspci"].contains(&g["source"].as_str().unwrap()), "source: {}", g["source"]);
        }
        eprintln!("[gpu-test] NVML unreachable; verified fallback enumeration only");
    }
}

#[test]
fn proc_sample_and_kill_roundtrip() {
    let mut hub = Hub::new();
    let _ = hub.procs.sample();
    sleep(700);
    let (procs, self_uid) = hub.procs.sample();
    assert!(procs.len() > 300, "process count: {}", procs.len());
    assert!(self_uid > 0);

    // self is listed
    let me = procs.iter().find(|p| p.is_self).expect("self in list");
    assert_eq!(me.pid as u32, std::process::id(), "self pid");
    assert!(!me.name.is_empty());

    // every proc has a resolved user and sane fields
    for p in &procs {
        assert!(!p.user.is_empty(), "user for {}", p.pid);
        assert!(p.cpu >= 0.0 && p.cpu <= 100.0);
        assert_eq!(p.state.to_string().len(), 1);
    }

    // spawn a sleep, watch it appear, kill it, watch it go
    let mut child = std::process::Command::new("sleep").arg("300").spawn().expect("spawn sleep");
    let pid = child.id();
    let mut found = false;
    for _ in 0..10 {
        sleep(500);
        let (list, _) = hub.procs.sample();
        if let Some(p) = list.iter().find(|p| p.pid == pid) {
            found = true;
            assert_eq!(p.name, "sleep", "name: {}", p.name);
            let expect_user = std::env::var("USER").unwrap_or_default();
            assert_eq!(p.user, expect_user, "user resolved: {} (expect {expect_user})", p.user);
            break;
        }
    }
    assert!(found, "sleep {pid} appeared in list");

    let r = task_manager_lib::collectors::procs::kill_process(pid, false);
    assert_eq!(r["ok"], true, "kill ok: {r}");
    let mut gone = false;
    for _ in 0..10 {
        sleep(500);
        let (list, _) = hub.procs.sample();
        if !list.iter().any(|p| p.pid == pid) {
            gone = true;
            break;
        }
    }
    assert!(gone, "sleep {pid} gone after kill");
    let _ = child.wait();

    // killing a nonexistent pid -> ESRCH
    let r = task_manager_lib::collectors::procs::kill_process(999_999_999, false);
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"], "ESRCH", "{r}");
    // pid 1 -> EPERM
    let r = task_manager_lib::collectors::procs::kill_process(1, false);
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"], "EPERM", "{r}");
}

#[test]
fn proc_io_own_uid_visible() {
    // own-uid processes must carry diskBs after real I/O
    let mut hub = Hub::new();
    let mut child = std::process::Command::new("dd")
        .args(["if=/dev/zero", "of=/tmp/tm_tauri_io_test", "bs=1M", "count=20000", "conv=fsync", "status=none"])
        .spawn()
        .expect("spawn dd");
    let pid = child.id();
    sleep(1200);
    let (list, _) = hub.procs.sample();
    let _dd = list.iter().find(|p| p.pid == pid).expect("dd proc in list");
    // first delta may be null (needs 300ms+), sample again
    sleep(900);
    let (list, _) = hub.procs.sample();
    let dd = list.iter().find(|p| p.pid == pid).expect("dd proc still in list");
    assert!(dd.disk_bs.is_some(), "own-uid io visible (diskBs={:?}, cpu={})", dd.disk_bs, dd.cpu);
    std::process::Command::new("rm").arg("/tmp/tm_tauri_io_test").status().ok();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn perf_payload_top_level_shape() {
    let mut hub = Hub::new();
    sleep(400);
    let v = hub.performance();
    assert!(v["ts"].as_u64().unwrap() > 0);
    assert!(!v["host"].as_str().unwrap().is_empty());
    assert_eq!(v["platform"], "linux");
    for key in ["cpu", "memory", "disks", "gpus", "nets"] {
        assert!(v[key].is_object() || v[key].is_array(), "{key} present");
    }
    assert!(v["cpu"]["model"].as_str().unwrap().contains("i9"), "cpu model: {}", v["cpu"]["model"]);
}
