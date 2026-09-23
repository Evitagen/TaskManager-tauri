// TM_VERIFY=1 mode: drive the real UI (click through tabs, dump visible text)
// and run the end-task flow against a spawned `sleep` child. Screenshots use a
// root-window capture cropped to this app's X window (Tauri v2 exposes no
// window capture on Linux, and direct XGetImage on a GL webkit window can be
// stale under a compositor). Verify mode retitles the window to
// "Task Manager [Tauri Verify]" so it is unambiguous even while the Electron
// reference app (same title) is running.

use std::io::Write;
use std::time::Duration;

use tauri::Manager;

/// Verify-mode window title: keeps this app's X window unambiguous next to
/// the Electron reference app, which uses the same plain title.
pub const VERIFY_TITLE: &str = "Task Manager [Tauri Verify]";

/// Find the X window id of this app via xprop (no xdotool on this host).
fn find_window_id() -> Option<String> {
    let list = crate::collectors::util::run("xprop", &["-root", "_NET_CLIENT_LIST"], 2000)?;
    for id in list.split(',').map(|s| s.trim().to_string()) {
        if id.is_empty() {
            continue;
        }
        if let Some(name) = crate::collectors::util::run("xprop", &["-id", &id, "_NET_WM_NAME"], 1000) {
            if name.contains(VERIFY_TITLE) {
                return Some(id);
            }
        }
    }
    None
}

fn shoot(win: &tauri::WebviewWindow, win_id: &str, path: &str) {
    // Screenshot via root-window capture + crop to this X window's geometry:
    // direct XGetImage on a GL-backed webkit window can return a stale pixmap
    // under a compositor; the root capture reflects the composited truth.
    // Force a fresh frame first (1 px size wiggle).
    let _ = win.set_size(tauri::LogicalSize::new(1239.0, 800.0));
    std::thread::sleep(Duration::from_millis(120));
    let _ = win.set_size(tauri::LogicalSize::new(1240.0, 800.0));
    std::thread::sleep(Duration::from_millis(300));

    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let xg = here.join("shots/xg");
    if !xg.exists() {
        // build the geometry probe on demand (needs gcc + libX11 headers)
        let _ = crate::collectors::util::run(
            "gcc",
            &[
                "-O2",
                "-o",
                &xg.to_string_lossy(),
                &here.join("shots/xg.c").to_string_lossy(),
                "-lX11",
            ],
            15000,
        );
        if !xg.exists() {
            eprintln!("[verify] shots/xg missing and could not be built (needs gcc + libX11 dev headers)");
            return;
        }
    }
    let geom = crate::collectors::util::run(xg.to_str().unwrap(), &[win_id], 5000);
    let (x, y, w, h) = match geom {
        Some(out) if out.trim().split_ascii_whitespace().count() == 4 => {
            let it: Vec<&str> = out.trim().split_ascii_whitespace().collect();
            (it[0].to_string(), it[1].to_string(), it[2].to_string(), it[3].to_string())
        }
        _ => {
            eprintln!("[verify] geometry probe failed for {win_id}: {geom:?}");
            return;
        }
    };
    let root = here.join("shots/.root_tmp.png").to_string_lossy().to_string();
    if crate::collectors::util::run("import", &["-window", "root", &root], 15000).is_none() {
        eprintln!("[verify] root capture failed");
        return;
    }
    let crop = format!("{w}x{h}+{x}+{y}");
    if crate::collectors::util::run("magick", &[&root, "-crop", &crop, "+repage", path], 10000).is_none() {
        eprintln!("[verify] crop failed");
        return;
    }
    let _ = std::fs::remove_file(&root);
    if std::path::Path::new(path).exists() {
        eprintln!("[verify] screenshot: {path} (root-cropped at {x},{y} {w}x{h})");
    } else {
        eprintln!("[verify] no output file: {path}");
    }
}

/// TM_VERIFY_LAYOUT=1: dump bounding rects of all layout-relevant elements and exit.
pub fn run_layout(app: &tauri::AppHandle) {
    // retitle immediately (the window exists by setup time) so external tools
    // can find this window unambiguously from the start
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_title(VERIFY_TITLE);
    }
    std::thread::sleep(Duration::from_millis(8_000));
    let Some(win) = app.get_webview_window("main") else {
        eprintln!("[layout] no main window");
        app.exit(1);
        return;
    };
    let js = r#"(function () {
      const sel = ['#page','#content','.shell','.nav','.titlebar','.ov-row','.section','.graph-box','canvas'];
      const out = [];
      const seen = new Set();
      for (const s of sel) {
        for (const el of document.querySelectorAll(s)) {
          const key = (el.id ? '#' + el.id : s) + el.className;
          if (seen.has(key)) continue;
          seen.add(key);
          const r = el.getBoundingClientRect();
          const cs = getComputedStyle(el);
          out.push((el.id ? '#' + el.id : s) + ' [' + el.className + '] rect=' + [r.x|0, r.y|0, r.width|0, r.height|0].join(',') + ' disp=' + cs.display + ' vis=' + cs.visibility + ' pos=' + cs.position);
        }
      }
      const page = document.getElementById('page');
      const content = document.getElementById('content');
      const cs = getComputedStyle(content);
      page.classList.add('measuring');
      const natural = page.scrollHeight;
      const measuringPageH = page.getBoundingClientRect().height;
      page.classList.remove('measuring');
      const avail = content.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom);
      const bodyH = document.body.getBoundingClientRect().height;
      window.__TAURI_INTERNALS__.invoke('debug_log', { text: 'LAYOUT ' + out.join(' || ') });
      window.__TAURI_INTERNALS__.invoke('debug_log', { text: 'MEASURE contentClient=' + content.clientHeight + ' avail=' + avail + ' natural=' + natural + ' measuringPageH=' + measuringPageH.toFixed(1) + ' livePageH=' + page.getBoundingClientRect().height.toFixed(1) + ' bodyH=' + bodyH.toFixed(1) + ' winH=' + window.innerHeight + ' winW=' + window.innerWidth + ' dpr=' + window.devicePixelRatio + ' transform=' + JSON.stringify(page.style.transform) });
      // per-core (right-click) flow probe on the cpu tab
      const clickTab = (sec) => { const b = [...document.querySelectorAll('.nav-item')].find(x => x.dataset.sec === sec); if (b) b.click(); };
      clickTab('cpu');
      setTimeout(() => {
        const box = document.getElementById('cpu-graph-box');
        const r = box.getBoundingClientRect();
        box.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.x + 100, clientY: r.y + 60 }));
        setTimeout(() => {
          const menu = document.getElementById('cpu-ctx-menu');
          const menuOpen = menu.classList.contains('open');
          const label = (menu.querySelector('.ctx-item') || {}).textContent || '';
          const item = menu.querySelector('.ctx-item');
          if (item) item.click();
          setTimeout(() => {
            const cells = document.querySelectorAll('.core-cell').length;
            const host = document.getElementById('cores-host');
            const hr = host.getBoundingClientRect();
            const hostVis = getComputedStyle(host).display;
            window.__TAURI_INTERNALS__.invoke('debug_log', { text: 'CORE menuOpen=' + menuOpen + ' label=' + JSON.stringify(label) + ' cells=' + cells + ' host=[' + [hr.x|0,hr.y|0,hr.width|0,hr.height|0].join(',') + '] disp=' + hostVis });
          }, 700);
        }, 300);
      }, 900);
    })()"#;
    let _ = win.eval(js);
    std::thread::sleep(Duration::from_millis(4_000));
    app.exit(0);
}

pub fn run(app: &tauri::AppHandle) {
    // warm-up: let the webview load and the graphs collect data
    std::thread::sleep(Duration::from_millis(10_000));
    let Some(win) = app.get_webview_window("main") else {
        eprintln!("[verify] no main window");
        app.exit(1);
        return;
    };
    let _ = win.set_title(VERIFY_TITLE);
    let _ = std::fs::create_dir_all("shots");

    // find the X window (retry: it appears shortly after setup)
    let mut win_id = None;
    for _ in 0..20 {
        if win_id.is_none() {
            win_id = find_window_id();
        }
        if win_id.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!("[verify] window id: {win_id:?}");

    // kill-test target
    let mut child = std::process::Command::new("sleep").arg("300").spawn().ok();
    let test_pid = child.as_ref().map(|c| c.id()).unwrap_or(0);

    for tab in ["overview", "cpu", "mem", "gpu", "disk", "net", "tasks"] {
        let js = format!(
            "(()=>{{ const b=[...document.querySelectorAll('.nav-item')].find(x=>x.dataset.sec==='{tab}'); if(b) b.click(); }})()"
        );
        let _ = win.eval(&js);
        std::thread::sleep(Duration::from_millis(1500));

        let dump = format!(
            r#"(() => {{
              const secs = [...document.querySelectorAll('.section')].filter(s => s.style.display !== 'none').map(s => s.id).join(',');
              const parts = secs ? secs.split(',').map(id => ((document.getElementById(id) || {{}}).innerText || '').replace(/\n+/g, ' | ').slice(0, 320)) : [];
              const p = document.getElementById('page');
              return window.__TAURI_INTERNALS__.invoke('debug_log', {{
                text: 'DUMP {tab} vis=' + secs + ' transform=' + (p.style.transform || 'fill-100%') + ' :: ' + parts.join(' @@ ')
              }});
            }})()"#
        );
        let _ = win.eval(&dump);
        std::thread::sleep(Duration::from_millis(500));

        shoot(&win, win_id.as_deref().unwrap_or(""), &format!("shots/tauri_{tab}.png"));
    }

    // end-task flow against the spawned sleep child
    let js = format!(
        r#"(async () => {{
          const log = (t) => window.__TAURI_INTERNALS__.invoke('debug_log', {{ text: t }});
          const sleep = (ms) => new Promise(r => setTimeout(r, ms));
          const row = document.querySelector('tr[data-pid="{pid}"]');
          if (!row) {{ log('KILLTEST row not found pid={pid}'); return; }}
          row.click();
          await sleep(400);
          const btn = document.getElementById('btn-end-task');
          if (!btn || btn.disabled) {{ log('KILLTEST end button not enabled'); return; }}
          btn.click();
          await sleep(400);
          const modal = document.getElementById('modal');
          if (modal.hidden) {{ log('KILLTEST modal not shown'); return; }}
          document.getElementById('modal-ok').click();
          await sleep(2000);
          const gone = !document.querySelector('tr[data-pid="{pid}"]');
          log('KILLTEST ' + (gone ? 'PASS' : 'FAIL') + ' pid={pid}');
        }})()"#,
        pid = test_pid
    );
    let _ = win.eval(&js);
    std::thread::sleep(Duration::from_millis(3500));

    let reaped = child
        .as_mut()
        .and_then(|c| c.try_wait().ok().flatten())
        .map(|s| s.success());
    eprintln!("[verify] KILLTEST child reaped={reaped:?} pid={test_pid}");
    if let Some(mut c) = child.take() {
        let _ = c.kill();
    }

    std::thread::sleep(Duration::from_millis(500));
    let mut out = std::io::stderr();
    let _ = writeln!(out, "[verify] done");
    app.exit(0);
}
