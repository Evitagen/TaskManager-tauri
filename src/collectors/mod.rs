pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod logger;
pub mod mem;
pub mod net;
pub mod procs;
pub mod util;

use serde_json::{json, Value};

use crate::collectors::cpu::CpuSampler;
use crate::collectors::disk::DiskSampler;
use crate::collectors::gpu::GpuCollector;
use crate::collectors::net::NetSampler;
use crate::collectors::procs::ProcSampler;
use crate::collectors::util::now_ms;

pub struct Hub {
    pub cpu: CpuSampler,
    pub disk: DiskSampler,
    pub gpu: GpuCollector,
    pub net: NetSampler,
    pub procs: ProcSampler,
}

fn hostname() -> String {
    let mut buf = [0i8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
    if rc == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        return String::from_utf8_lossy(&buf[..len].iter().map(|&b| b as u8).collect::<Vec<_>>()).into_owned();
    }
    "unknown".into()
}

impl Hub {
    pub fn new() -> Self {
        Self {
            cpu: CpuSampler::new(),
            disk: DiskSampler::new(),
            gpu: GpuCollector::new(),
            net: NetSampler::new(),
            procs: ProcSampler::new(),
        }
    }

    /// Perf payload consumed by the frontend (the `perf_sample` command).
    pub fn performance(&mut self) -> Value {
        let info = self.cpu.info().clone();
        let cpu_sample = self.cpu.sample();
        let mem_sample = mem::sample();
        let disks = self.disk.sample();
        let gpus = self.gpu.sample();
        let nets = self.net.sample();

        let cpu_obj = match cpu_sample {
            Some(s) => {
                let mut v = serde_json::to_value(&s).unwrap_or(Value::Null);
                if v.is_object() {
                    v["model"] = json!(info.model);
                    v["logical"] = json!(info.logical);
                    v["physical"] = json!(info.physical);
                    v["baseFreq"] = json!(info.base_freq);
                    v["maxFreq"] = json!(info.max_freq);
                }
                v
            }
            None => Value::Null,
        };

        json!({
            "ts": now_ms(),
            "host": hostname(),
            "platform": "linux",
            "cpu": cpu_obj,
            "memory": mem_sample,
            "disks": disks,
            "gpus": gpus,
            "nets": nets,
        })
    }

    /// Process payload consumed by the frontend (the `proc_sample` command).
    pub fn processes(&mut self) -> Value {
        let (procs, self_uid) = self.procs.sample();
        json!({
            "ts": now_ms(),
            "procs": procs,
            "selfUid": self_uid,
        })
    }
}
