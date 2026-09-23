# Task Manager — Tauri v2 port

A self-contained [Tauri](https://tauri.app) v2 port of the Electron task manager
in the parent directory (`../`). The Rust side re-implements the collector logic
from the Electron app's `lib/` with **identical JSON contracts**, and the web UI
is reused almost verbatim — the only frontend change is a small `bridge.js` that
maps the same `window.api` surface onto Tauri's `__TAURI_INTERNALS__.invoke`.

The Electron app is left untouched; everything here lives in this folder.

## Screenshots

| Overview | CPU | GPU |
|----------|-----|-----|
| ![Overview](screenshots/overview.png) | ![CPU](screenshots/cpu.png) | ![GPU](screenshots/gpu.png) |

| Disks | Network | Running tasks |
|-------|---------|---------------|
| ![Disks](screenshots/disk.png) | ![Network](screenshots/net.png) | ![Running tasks](screenshots/tasks.png) |

## Layout

```
tauri/
├── Cargo.toml / Cargo.lock      # crate (lib name: task_manager_lib) + deps
├── build.rs                     # tauri_build + AppManifest (declares commands)
├── tauri.conf.json              # window 1240×800 (min 940×620), no decorations,
│                                #   identifier com.taskmanager.tauri, frontendDist=frontend
├── capabilities/default.json    # core:default + window perms + our command perms
├── icons/icon.png
├── src/
│   ├── main.rs                  # thin entry: task_manager_lib::run()
│   ├── lib.rs                   # tauri Builder, commands, verify-mode hooks
│   ├── verify.rs                # TM_VERIFY / TM_VERIFY_LAYOUT self-check harness
│   └── collectors/
│       ├── mod.rs               # Hub: performance()/processes() -> exact JSON
│       ├── cpu.rs               # /proc/stat deltas, per-core, freq, load, uptime
│       ├── mem.rs               # /proc/meminfo (kB -> bytes, cached/buffers/swap)
│       ├── disk.rs              # /proc/diskstats deltas + statfs mount usage
│       ├── net.rs               # /proc/net/dev deltas, physical-iface only
│       ├── gpu.rs               # nvidia-smi (NVML) w/ drm + lspci fallbacks
│       ├── procs.rs             # /proc/<pid> snapshot + kill (sigterm/sigkill)
│       └── util.rs              # read helpers + timeout-bounded process runner
├── frontend/
│   ├── index.html / style.css   # copied from the Electron renderer
│   ├── app.js / graph.js        # copied (app.js: +rAF fallback in fitPage)
│   └── bridge.js                # NEW: window.api -> Tauri invoke
├── tests/collectors.rs          # integration tests for the collectors
├── screenshots/                 # README screenshots (one per tab)
├── shots/                       # verify-mode screenshots + xg/mouse X11 helpers
└── .toolchain/                  # in-project rustup/cargo homes (self-contained)
```

## Building & running

A Rust toolchain is vendored in `.toolchain/` (RUSTUP_HOME/CARGO_HOME live there,
so no system-wide rust is required). Every command below is run from this folder
with the toolchain prepended:

```sh
cd tauri
export RUSTUP_HOME=$PWD/.toolchain/rustup
export CARGO_HOME=$PWD/.toolchain/cargo
export PATH=$CARGO_HOME/bin:$PATH

cargo build            # debug binary -> target/debug/task-manager
cargo run              # run it
cargo test             # collector integration tests
```

Release build (LTO + strip, per `[profile.release]`):

```sh
cargo build --release  # -> target/release/task-manager
```

Dependencies used by the build/run on this host: the project toolchain, plus
`libwebkit2gtk` (WebKitGTK 2.52) for the webview. The verify harness (below)
additionally uses `xprop`, ImageMagick (`import`, `magick`), and `gcc` +
libX11 dev headers (to build `shots/xg`, the X-geometry probe).

## Verification harness

Two env-gated modes are wired into `lib.rs` (neither affects normal use):

- `TM_VERIFY=1 ./target/debug/task-manager`
  Warms up, then drives the **real UI**: clicks through all six tabs
  (Overview, CPU, GPU, Disk, Net, Tasks), dumps each visible section's text and
  the page transform to stderr, screenshots each tab, then exercises the
  end-task flow against a spawned `sleep` child (row → End task → confirm) and
  checks the kill result. Prints `KILLTEST PASS/FAIL` and exits.

- `TM_VERIFY_LAYOUT=1 ./target/debug/task-manager`
  Dumps layout geometry (rects/display of the page, sections, graphs, canvas),
  the fit-scaler's measured vs. available height, and the per-core right-click
  flow (context menu → "Show logical cores" → 28 core cells). Exits.

Screenshots are written to `shots/tauri_<tab>.png`. Because Tauri v2 exposes no
window-capture API on Linux, the harness captures the **root window** with
`import -window root` and crops to this app's X window geometry (queried by the
`shots/xg` probe). This is the composited on-screen truth. Verify mode retitles
the window to `Task Manager [Tauri Verify]` so it is unambiguous even while the
Electron reference app (same plain title) is running.

## JSON contract (identical to the Electron renderer)

`perf_sample` → `{ ts, host, platform, cpu{…}, memory{…}, disks[], gpus[], nets[] }`
`proc_sample` → `{ ts, procs[{pid,name,state,uid,user,cpu,mem,diskBs,netBs,cmdline,isApp,isSelf}], selfUid }`
`meta_get`    → `{ app, version, platform, arch, hostname }`
`proc_kill`   → `{ ok, error? }`

Field names are camelCase (`serde rename_all`) and match every consumer in
`frontend/app.js`. Rates (B/s, %, MHz, GHz) are computed with the same
delta-over-time semantics as the original `lib/`.

## Known differences / notes

- **`WEBKIT_DISABLE_COMPOSITING_MODE=1` is set in `run()`** (before the webview
  is created). On this host (picom with the xrender backend), WebKitGTK's
  accelerated GL compositing path does not reach the window pixmap / compositor,
  which would leave the window a flat background. Plain compositing mode renders
  correctly and is what the screenshots above were captured with.
- **`fitPage()` rAF fallback.** WebKitGTK pauses `requestAnimationFrame` while
  the webview is not being composited, so the fit-scaler also runs on a short
  timeout. Behaviour is unchanged in Chromium (rAF path wins, timeout is a no-op).
- **GPU source.** Primary path is `nvidia-smi` (NVML); if that is unavailable the
  collector falls back to DRM sysfs and `/proc/driver/nvidia` + `lspci`, and
  marks `telemetry:false`. On this host NVML is live (2× RTX 3090).
- **`isApp` / `isSelf` window tagging** depends on `wmctrl`, which is not
  installed here; those fields are therefore empty (same as the JS app without
  wmctrl). `isSelf` is always populated.
- **CPU "Speed" may be null** on CPUs whose `cpuinfo` lacks `cpu MHz` (e.g. this
  Skylake-X); the UI then shows `—`, exactly like the original.
- **Window drag & resize.** WebKitGTK ignores Chromium's `-webkit-app-region:
  drag` (and a `decorations:false` window has no OS-side resize borders either),
  so `bridge.js` drives the core `plugin:window|start_dragging` command on
  titlebar mousedown and `plugin:window|start_resize_dragging` from eight
  invisible edge/corner hit-strips with the matching cursors. The 940×620
  minimum size is still enforced by the window manager.

## Commands (Tauri invoke surface)

| Command       | Args            | Returns                          |
|---------------|-----------------|----------------------------------|
| `perf_sample` | —               | perf payload (above)             |
| `proc_sample` | —               | procs payload (above)            |
| `proc_kill`   | `{pid, force}`  | `{ok, error?}` (EPERM if pid≤1 or self) |
| `meta_get`    | —               | meta payload                     |
| `debug_log`   | `{text}`        | — (verify-mode logging hook)     |

Window controls use the `plugin:window|…` core commands (`minimize`,
`maximize`, `unmaximize`, `is_maximized`, `close`, `start_dragging`,
`start_resize_dragging`) with `{label:'main'}`.
