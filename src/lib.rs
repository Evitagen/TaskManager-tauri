pub mod collectors;
mod verify;

use std::sync::Mutex;

use collectors::procs;
use collectors::Hub;
use serde_json::{json, Value};

#[tauri::command]
fn perf_sample(state: tauri::State<'_, Mutex<Hub>>) -> Value {
    let mut hub = state.lock().unwrap_or_else(|e| e.into_inner());
    hub.performance()
}

#[tauri::command]
fn proc_sample(state: tauri::State<'_, Mutex<Hub>>) -> Value {
    let mut hub = state.lock().unwrap_or_else(|e| e.into_inner());
    hub.processes()
}

#[tauri::command]
fn proc_kill(pid: u32, force: bool) -> Value {
    // refuse to let the app kill itself or init (same guard as main.js)
    if pid <= 1 || pid == std::process::id() {
        return json!({ "ok": false, "error": "EPERM (protected)" });
    }
    procs::kill_process(pid, force)
}

#[tauri::command]
fn meta_get() -> Value {
    let hostname = {
        let mut buf = [0i8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
        if rc == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..len].iter().map(|&b| b as u8).collect::<Vec<_>>()).into_owned()
        } else {
            "unknown".into()
        }
    };
    json!({
        "app": "Task Manager",
        "version": env!("CARGO_PKG_VERSION"),
        "platform": "linux",
        "arch": if cfg!(target_arch = "x86_64") { "x64" } else { std::env::consts::ARCH },
        "hostname": hostname,
    })
}

/// Verify-mode logging hook (renderer -> console).
#[tauri::command]
fn debug_log(text: String) {
    eprintln!("[debug] {text}");
}

pub fn run() {
    // WebKitGTK's accelerated (GL) compositing path does not reach the X
    // window pixmap / compositor on this host (picom xrender backend), which
    // leaves the window a flat background. Force the plain compositing mode
    // before the webview is created; the env is read by WebKit in-process.
    std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");

    let mut builder = tauri::Builder::default()
        .manage(Mutex::new(Hub::new()))
        .invoke_handler(tauri::generate_handler![
            perf_sample,
            proc_sample,
            proc_kill,
            meta_get,
            debug_log
        ]);
    if std::env::var("TM_VERIFY_LAYOUT").ok().map(|v| !v.is_empty()).unwrap_or(false) {
        builder = builder.setup(|app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || verify::run_layout(&handle));
            Ok(())
        });
    }
    if std::env::var("TM_VERIFY").ok().map(|v| !v.is_empty()).unwrap_or(false) {
        builder = builder.setup(|app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || verify::run(&handle));
            Ok(())
        });
    }
    builder
        .run(tauri::generate_context!())
        .expect("error while running Task Manager (Tauri)");
}
