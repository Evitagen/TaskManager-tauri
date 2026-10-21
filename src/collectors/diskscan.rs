// Disk space scanner — a du-style background walk of a directory tree that
// feeds the Disks-tab treemap ("click a disk → grid of the biggest items").
//
// Semantics (mirroring `du -x`):
//   · never follow symlinks (symlinks count as their own tiny entry)
//   · directories on a different filesystem than the scan root are skipped
//     (their subtree is not descended into)
//   · per-directory the largest CHILD_CAP entries (dirs and files mixed) are
//     kept; the tail is folded into an "… N more" bucket on the frontend
//
// One coordinator thread walks the scan root and hands every top-level
// directory to a small worker pool (WORKERS threads) that scans whole
// subtrees in parallel. Progress is published into a shared state that the
// renderer polls with `disk_scan_status` (see the frontend bridge).

use std::collections::BinaryHeap;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use super::util::now_ms;

/// Largest entries kept per directory; the rest is counted as omitted.
const CHILD_CAP: usize = 250;
/// Largest files tracked globally (the flat "Top files" view).
const TOP_KEEP: usize = 500;
/// Worker threads scanning top-level subtrees in parallel.
const WORKERS: usize = 6;
/// Progress publication interval (the renderer polls at ~300 ms).
const REPORT_MS: u64 = 250;

#[derive(Serialize, Clone)]
pub struct SNode {
    pub name: String,
    /// "dir" | "file"
    pub kind: String,
    /// Total bytes (file: its size; dir: hidden + children subtrees).
    pub size: u64,
    /// Largest children (dirs and files mixed), sorted descending by size.
    pub children: Vec<SNode>,
    /// Entries dropped by the per-directory cap.
    pub omitted: u64,
    /// Bytes of those dropped entries (kept so totals stay exact).
    pub hidden: u64,
}

fn node_dir(name: &str) -> SNode {
    SNode { name: name.into(), kind: "dir".into(), size: 0, children: Vec::new(), omitted: 0, hidden: 0 }
}

fn node_file(name: &str, size: u64) -> SNode {
    SNode { name: name.into(), kind: "file".into(), size, children: Vec::new(), omitted: 0, hidden: 0 }
}

/// One of the globally largest files, with its path relative to the scan root.
#[derive(Serialize)]
struct TopFile {
    name: String,
    size: u64,
    path: String,
}

/// Bounded min-heap of the TOP_KEEP largest files seen (ordered by size so
/// the smallest kept entry is the one to evict).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct TopHeapItem(TopFile);

impl PartialEq for TopFile {
    fn eq(&self, o: &Self) -> bool {
        self.size == o.size && self.path == o.path
    }
}
impl Eq for TopFile {}
impl PartialOrd for TopFile {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
}
impl Ord for TopFile {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.size.cmp(&o.size).then_with(|| self.path.cmp(&o.path))
    }
}

/// Track a candidate file in the global top list. Cheap early-out: small
/// files (the overwhelming majority) only pay one mutex probe, no allocation.
fn top_push(sh: &ScanShared, scan_root: &Path, abs: &Path, size: u64) {
    let mut t = sh.top.lock().unwrap_or_else(|e| e.into_inner());
    if t.len() >= TOP_KEEP {
        let keep = match t.peek() {
            Some(std::cmp::Reverse(item)) => size > item.0.size,
            None => true,
        };
        if !keep {
            return;
        }
        t.pop();
    }
    let rel = abs.strip_prefix(scan_root).unwrap_or(abs);
    t.push(std::cmp::Reverse(TopHeapItem(TopFile {
        name: abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        size,
        path: rel.to_string_lossy().into_owned(),
    })));
}

/// Keep the CHILD_CAP largest children of a directory; sizes of dropped
/// entries accumulate into `parent.hidden` so the parent total stays exact.
fn add_child(parent: &mut SNode, count: &mut u64, node: SNode) {
    *count += 1;
    if parent.children.len() < CHILD_CAP {
        parent.children.push(node);
        return;
    }
    let mut mi = 0;
    for (i, c) in parent.children.iter().enumerate().skip(1) {
        if c.size < parent.children[mi].size {
            mi = i;
        }
    }
    if node.size > parent.children[mi].size {
        parent.hidden += parent.children[mi].size;
        parent.children[mi] = node;
    } else {
        parent.hidden += node.size;
    }
}

fn finalize(node: &mut SNode, count: u64) {
    node.children.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    node.omitted = count.saturating_sub(node.children.len() as u64);
    node.size = node.hidden + node.children.iter().map(|c| c.size).sum::<u64>();
}

/// Shared scan state: atomics for the hot counters (every file touched),
/// a mutex for status fields + the finished tree, an atomic cancel flag.
pub struct ScanShared {
    cancel: AtomicBool,
    files: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    other_fs: AtomicU64,
    top: Mutex<BinaryHeap<std::cmp::Reverse<TopHeapItem>>>,
    inner: Mutex<Inner>,
}

struct Inner {
    scanning: bool,
    path: String,
    started_ms: u64,
    current: String,
    done: bool,
    cancelled: bool,
    error: Option<String>,
    tree: Option<SNode>,
    /// Serialized tree, built lazily on the first `disk_scan_tree` call.
    tree_json: Option<serde_json::Value>,
    /// Top-files array, built once when the scan finishes.
    top_files: Option<serde_json::Value>,
}

impl ScanShared {
    pub fn new() -> Self {
        Self {
            cancel: AtomicBool::new(false),
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            other_fs: AtomicU64::new(0),
            top: Mutex::new(BinaryHeap::new()),
            inner: Mutex::new(Inner {
                scanning: false,
                path: String::new(),
                started_ms: 0,
                current: String::new(),
                done: true,
                cancelled: false,
                error: None,
                tree: None,
                tree_json: None,
                top_files: None,
            }),
        }
    }
}

/* ── iterative DFS over one subtree (worker-side) ───────────────────────── */

struct Frame {
    path: PathBuf,
    node: SNode,
    parent: Option<usize>,
    count: u64,
    it: std::fs::ReadDir,
}

/// Scan the subtree at `root` (already known to be a directory on `root_dev`)
/// and send the finished node (or None on cancel/error) over `out`.
fn scan_subtree(root: &PathBuf, name: &str, scan_root: &Path, root_dev: u64, sh: &ScanShared, out: &mpsc::Sender<Option<SNode>>) {
    let it = match fs::read_dir(root) {
        Ok(i) => i,
        Err(_) => {
            sh.errors.fetch_add(1, Ordering::Relaxed);
            let _ = out.send(None);
            return;
        }
    };
    let mut stack: Vec<Frame> = vec![Frame {
        path: root.clone(),
        node: node_dir(name),
        parent: None,
        count: 0,
        it,
    }];
    let mut ticks = 0u64;
    let mut last_report = now_ms();

    while let Some(top) = stack.len().checked_sub(1) {
        if sh.cancel.load(Ordering::Relaxed) {
            let _ = out.send(None);
            return;
        }
        let ev = stack[top].it.next();
        match ev {
            None => {
                // finished this directory — fold it into its parent
                let mut frame = stack.pop().unwrap();
                finalize(&mut frame.node, frame.count);
                match frame.parent {
                    Some(pi) => {
                        let f = &mut stack[pi];
                        add_child(&mut f.node, &mut f.count, frame.node);
                    }
                    None => {
                        let _ = out.send(Some(frame.node));
                        return;
                    }
                }
            }
            Some(Err(_)) => {
                sh.errors.fetch_add(1, Ordering::Relaxed);
            }
            Some(Ok(entry)) => {
                ticks += 1;
                match entry.file_type() {
                    Err(_) => {
                        sh.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(ft) if ft.is_dir() => {
                        let md = match entry.metadata() {
                            Ok(m) => m,
                            Err(_) => {
                                sh.errors.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        };
                        if md.dev() != root_dev {
                            // du -x: never cross filesystems
                            sh.other_fs.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let child_path = stack[top].path.join(entry.file_name());
                        match fs::read_dir(&child_path) {
                            Err(_) => {
                                sh.errors.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(it) => {
                                let name = entry.file_name().to_string_lossy().into_owned();
                                stack.push(Frame {
                                    path: child_path,
                                    node: node_dir(&name),
                                    parent: Some(top),
                                    count: 0,
                                    it,
                                });
                            }
                        }
                    }
                    Ok(_) => {
                        // file or symlink: lstat size only, never follow
                        let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let abs = stack[top].path.join(&name);
                        top_push(sh, scan_root, &abs, sz);
                        let fr = &mut stack[top];
                        add_child(&mut fr.node, &mut fr.count, node_file(&name, sz));
                        sh.files.fetch_add(1, Ordering::Relaxed);
                        sh.bytes.fetch_add(sz, Ordering::Relaxed);
                    }
                }
                if ticks % 256 == 0 {
                    let now = now_ms();
                    if now - last_report >= REPORT_MS {
                        last_report = now;
                        let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                        g.current = stack[top].path.display().to_string();
                    }
                }
            }
        }
    }
}

/* ── coordinator + worker pool ──────────────────────────────────────────── */

fn coordinator(sh: Arc<ScanShared>, root: PathBuf, root_dev: u64) {
    let display = root.display().to_string();
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| display.clone());

    let (tx, rx) = mpsc::channel::<Option<SNode>>();
    let queue: Arc<Mutex<VecDeque<(PathBuf, String)>>> = Arc::new(Mutex::new(VecDeque::new()));
    let pending = Arc::new(AtomicU64::new(0));

    // Walk the scan root here: files directly into the root node, every
    // top-level directory goes to the worker queue.
    let mut root_node = node_dir(&root_name);
    let mut root_count = 0u64;
    let mut n_dirs = 0usize;
    let mut root_err: Option<String> = None;

    match fs::read_dir(&root) {
        Err(e) => root_err = Some(format!("cannot read {}: {e}", display)),
        Ok(it) => {
            for entry in it {
                if sh.cancel.load(Ordering::Relaxed) {
                    root_err = Some("cancelled".into());
                    break;
                }
                match entry {
                    Err(_) => {
                        sh.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(e) => match e.file_type() {
                        Err(_) => {
                            sh.errors.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(ft) if ft.is_dir() => {
                            let md = match e.metadata() {
                                Ok(m) => m,
                                Err(_) => {
                                    sh.errors.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                            };
                            if md.dev() != root_dev {
                                sh.other_fs.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            let name = e.file_name().to_string_lossy().into_owned();
                            let child_path = root.join(&name);
                            queue.lock().unwrap_or_else(|err| err.into_inner()).push_back((child_path, name));
                            pending.fetch_add(1, Ordering::Relaxed);
                            n_dirs += 1;
                        }
                        Ok(_) => {
                            let sz = e.metadata().map(|m| m.len()).unwrap_or(0);
                            let name = e.file_name().to_string_lossy().into_owned();
                            let abs = root.join(&name);
                            top_push(&sh, &root, &abs, sz);
                            add_child(&mut root_node, &mut root_count, node_file(&name, sz));
                            sh.files.fetch_add(1, Ordering::Relaxed);
                            sh.bytes.fetch_add(sz, Ordering::Relaxed);
                        }
                    },
                }
            }
        }
    }

    if root_err.is_none() {
        // workers: each drains top-level directories from the queue and scans
        // the whole subtree alone (bounded depth via the heap-allocated frame
        // stack inside scan_subtree).
        let n_workers = WORKERS.min(n_dirs).max(1);
        let mut handles = Vec::new();
        for _ in 0..n_workers {
            let sh_w = sh.clone();
            let q_w = queue.clone();
            let tx_w = tx.clone();
            let p_w = pending.clone();
            let sr_w = root.clone();
            handles.push(std::thread::spawn(move || loop {
                let next = q_w.lock().unwrap_or_else(|err| err.into_inner()).pop_front();
                let Some((path, name)) = next else { break };
                scan_subtree(&path, &name, &sr_w, root_dev, &sh_w, &tx_w);
                p_w.fetch_sub(1, Ordering::Relaxed);
            }));
        }
        drop(tx); // coordinator holds no sender; only workers feed the channel

        let mut cancelled = false;
        while !cancelled && pending.load(Ordering::Relaxed) > 0 {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(Some(node)) => add_child(&mut root_node, &mut root_count, node),
                Ok(None) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if sh.cancel.load(Ordering::Relaxed) {
                        cancelled = true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // all workers finished → drain any stragglers already queued
        if !cancelled {
            while let Ok(msg) = rx.try_recv() {
                if let Some(node) = msg {
                    add_child(&mut root_node, &mut root_count, node);
                }
            }
            finalize(&mut root_node, root_count);
        }
        for h in handles {
            let _ = h.join();
        }

        // Global top files, largest first (ties broken by path for stability).
        let top_files = {
            let mut t = sh.top.lock().unwrap_or_else(|e| e.into_inner());
            let mut v: Vec<TopFile> = std::mem::take(&mut *t).into_vec().into_iter().map(|std::cmp::Reverse(i)| i.0).collect();
            v.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
            serde_json::to_value(&v).unwrap_or(serde_json::Value::Null)
        };

        let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.done = true;
        g.scanning = false;
        g.current.clear();
        g.cancelled = cancelled;
        g.tree = if cancelled { None } else { Some(root_node) };
        g.top_files = if cancelled { None } else { Some(top_files) };
        return;
    }

    // error / cancelled while walking the root
    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
    g.done = true;
    g.scanning = false;
    g.current.clear();
    g.cancelled = root_err.as_deref() == Some("cancelled");
    g.error = if g.cancelled { None } else { root_err };
    g.tree = None;
}

/* ── public API used by the Tauri commands ──────────────────────────────── */

/// Start a scan of `path`. Fails if one is already running or the path is
/// not a directory.
pub fn start(sh: &Arc<ScanShared>, path: &str) -> serde_json::Value {
    let md = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return serde_json::json!({ "ok": false, "error": format!("cannot access {path}: {e}") }),
    };
    if !md.is_dir() {
        return serde_json::json!({ "ok": false, "error": format!("{path} is not a directory") });
    }
    let root = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let root_dev = md.dev();
    {
        let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.scanning && !g.done {
            return serde_json::json!({ "ok": false, "error": "a scan is already running" });
        }
        *g = Inner {
            scanning: true,
            path: root.display().to_string(),
            started_ms: now_ms(),
            current: String::new(),
            done: false,
            cancelled: false,
            error: None,
            tree: None,
            tree_json: None,
            top_files: None,
        };
    }
    sh.cancel.store(false, Ordering::Relaxed);
    sh.files.store(0, Ordering::Relaxed);
    sh.bytes.store(0, Ordering::Relaxed);
    sh.errors.store(0, Ordering::Relaxed);
    sh.other_fs.store(0, Ordering::Relaxed);
    sh.top.lock().unwrap_or_else(|e| e.into_inner()).clear();

    let sh2 = sh.clone();
    std::thread::spawn(move || coordinator(sh2, root, root_dev));
    serde_json::json!({ "ok": true })
}

pub fn cancel(sh: &Arc<ScanShared>) {
    sh.cancel.store(true, Ordering::Relaxed);
}

/// Poll payload for the renderer. Deliberately tree-free: the full tree can
/// be tens of MB on a big mount, so it is fetched once via `tree()` only
/// when the user actually drills into a folder.
pub fn status(sh: &Arc<ScanShared>) -> serde_json::Value {
    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
    serde_json::json!({
        "scanning": g.scanning && !g.done,
        "done": g.done,
        "cancelled": g.cancelled,
        "error": g.error,
        "path": g.path,
        "current": g.current,
        "elapsedMs": if g.started_ms > 0 { now_ms().saturating_sub(g.started_ms) } else { 0 },
        "files": sh.files.load(Ordering::Relaxed),
        "bytes": sh.bytes.load(Ordering::Relaxed),
        "errors": sh.errors.load(Ordering::Relaxed),
        "otherFs": sh.other_fs.load(Ordering::Relaxed),
        "topFiles": g.top_files,
    })
}

/// The finished scan's directory tree, serialized once and cached.
pub fn tree(sh: &Arc<ScanShared>) -> serde_json::Value {
    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
    if !g.done || g.cancelled || g.error.is_some() {
        return serde_json::json!({ "ok": false, "error": "no completed scan" });
    }
    if g.tree_json.is_none() {
        if let Some(t) = &g.tree {
            g.tree_json = Some(serde_json::to_value(t).unwrap_or(serde_json::Value::Null));
        }
    }
    match &g.tree_json {
        Some(j) => serde_json::json!({ "ok": true, "tree": j.clone() }),
        None => serde_json::json!({ "ok": false, "error": "no tree" }),
    }
}
