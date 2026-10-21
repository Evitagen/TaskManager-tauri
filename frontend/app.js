// Task Manager renderer — tabbed dashboard:
// Overview (CPU + Memory + GPU) · CPU · Memory · GPU · Disks · Network · Tasks.
'use strict';

/* ── formatting helpers ─────────────────────────────────────────────────── */
const KB = 1024, MB = KB * 1024, GB = MB * 1024;
const fBytes = (b, perSec = false) => {
  if (b == null) return '—';
  const suf = perSec ? '/s' : '';
  const abs = Math.abs(b);
  if (abs >= GB) return (b / GB).toFixed(abs / GB >= 10 ? 0 : 1) + ' GB' + suf;
  if (abs >= MB) return (b / MB).toFixed(abs / MB >= 10 ? 0 : 1) + ' MB' + suf;
  if (abs >= KB) return (b / KB).toFixed(abs / KB >= 10 ? 0 : 1) + ' KB' + suf;
  return Math.round(b) + ' B' + suf;
};
const fPct = v => v == null ? '—' : (v >= 10 ? Math.round(v) : Math.round(v * 10) / 10) + '%';
const vCls = v => v == null ? 'v-dim' : v < 35 ? 'v-ok' : v < 75 ? 'v-warn' : 'v-bad';
const fUp = s => {
  if (s == null) return '—';
  const d = Math.floor(s / 86400), h = Math.floor(s % 86400 / 3600), m = Math.floor(s % 3600 / 60);
  return d ? `${d}d ${h}h ${m}m` : h ? `${h}h ${m}m` : `${m}m`;
};
const STATE_NAMES = { R: 'Running', S: 'Sleeping', D: 'Running', T: 'Suspended', Z: 'Zombie', I: 'Idle', X: 'Exited' };
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

/* ── refresh rates ──────────────────────────────────────────────────────── */
const PERF_MS = 500;    // CPU / memory / GPU / disk / network data (numbers + graph points)
const PROC_MS = 500;    // process list sampling
const HIST_WINDOW_MS = 90_000;  // graphs show this much history (90 s)

/* ── history store (shared arrays feeding the graphs) ───────────────────── */
// Points are {t, v}: timestamped so the 90 s time window slides on every
// redraw (once per data tick).
const PRUNE_MS = HIST_WINDOW_MS + 2_000;
const push = (arr, v) => {
  const t = performance.now();
  arr.push({ t, v: v == null ? 0 : v });
  if (arr.length > 8 && arr[0].t < t - PRUNE_MS) {
    let i = 0;
    while (i < arr.length && arr[i].t < t - PRUNE_MS) i++;
    if (i > 16) arr.splice(0, i);
  }
};

const hist = {
  cpuTotal: [], cpuCores: [],
  mem: [],
  disks: new Map(),   // id -> {active, read, write}
  gpus:  new Map(),   // id -> {util, memPct}
  nets:  new Map(),   // id -> {rx, tx}
};

function updateHist(p) {
  push(hist.cpuTotal, p.cpu.usage);
  if (!hist.cpuCores.length && p.cpu.perCore?.length)
    hist.cpuCores = p.cpu.perCore.map(() => []);
  p.cpu.perCore?.forEach((v, i) => hist.cpuCores[i] && push(hist.cpuCores[i], v));

  push(hist.mem, p.memory.total ? 100 * p.memory.inUse / p.memory.total : 0);

  for (const d of p.disks) {
    if (!hist.disks.has(d.id)) hist.disks.set(d.id, { active: [], read: [], write: [] });
    const h = hist.disks.get(d.id);
    push(h.active, d.active); push(h.read, d.readBs); push(h.write, d.writeBs);
  }
  for (const g of p.gpus) {
    if (!hist.gpus.has(g.id)) hist.gpus.set(g.id, { util: [], memPct: [] });
    const h = hist.gpus.get(g.id);
    if (g.telemetry) {
      push(h.util, g.util ?? 0);
      push(h.memPct, g.memTotal ? 100 * (g.memUsed || 0) / g.memTotal : 0);
    }
  }
  for (const n of p.nets) {
    if (!hist.nets.has(n.id)) hist.nets.set(n.id, { rx: [], tx: [] });
    const h = hist.nets.get(n.id);
    push(h.rx, n.rxBs); push(h.tx, n.txBs);
  }
}

/* ── graph registry ─────────────────────────────────────────────────────── */
const graphs = new Map();
function makeGraph(key, canvas, opts) {
  const old = graphs.get(key);
  if (old) { old._ro?.disconnect(); graphs.delete(key); }
  const g = new LineGraph(canvas, { ...opts, windowMs: HIST_WINDOW_MS });
  graphs.set(key, g);
  g.render();
  return g;
}
function destroyGraphs(prefix) {
  for (const [k, g] of [...graphs]) if (k.startsWith(prefix)) { g._ro?.disconnect(); graphs.delete(k); }
}
const allGraphs = () => [...graphs.values()];

/* ── title bar / window ─────────────────────────────────────────────────── */
document.getElementById('btn-min').onclick = () => window.api.win.min();
document.getElementById('btn-max').onclick = () => window.api.win.maxToggle();
document.getElementById('btn-close').onclick = () => window.api.win.close();

/* ── side nav = tabs: one resource per view (Overview = CPU + Memory + GPU) ─ */
const content = document.getElementById('content');
const ovRow = document.getElementById('ov-row');
const sections = [...document.querySelectorAll('.section')];
const togglables = [ovRow, ...sections];
const navItems = [...document.querySelectorAll('.nav-item')];
const TABS = {
  overview: ['ov-row', 'sec-cpu', 'sec-mem', 'sec-gpu'],
  cpu: ['ov-row', 'sec-cpu'],        // row collapses to one full-width column
  mem: ['ov-row', 'sec-mem'],
  gpu: ['sec-gpu'],
  disk: ['sec-disk'],
  net: ['sec-net'],
  tasks: ['sec-tasks'],
  log: ['sec-log'],
};
/* ── fit-to-window ──────────────────────────────────────────────────────── */
// Each tab fills the window via flex. If a tab's natural content is still
// taller than the window (many devices / small window), uniformly scale the
// whole page down so it fits exactly — no scrolling, ever.
const pageEl = document.getElementById('page');
function fitPage() {
  const doFit = () => {
    const cs = getComputedStyle(content);
    const avail = content.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom);
    if (!avail) return;
    pageEl.classList.add('measuring');
    const natural = pageEl.scrollHeight;
    pageEl.classList.remove('measuring');
    const k = natural > avail + 1 ? avail / natural : 1;
    // vertical-only compression: content always spans the full window width
    pageEl.style.transform = k < 1 ? `scaleY(${k})` : '';
    pageEl.style.transformOrigin = 'top center';
  };
  // rAF when it fires (Chromium); WebKitGTK pauses rAF while the webview is
  // not being composited, so also run on a short timeout as a fallback.
  let ran = false;
  requestAnimationFrame(() => { ran = true; doFit(); });
  setTimeout(() => { if (!ran) doFit(); }, 16);
}
window.addEventListener('resize', fitPage);

let activeTab = null;
function showTab(name, { scroll = true } = {}) {
  if (!TABS[name] || name === activeTab) return;
  activeTab = name;
  const ids = new Set(TABS[name]);
  for (const el of togglables) el.style.display = ids.has(el.id) ? '' : 'none';
  navItems.forEach(b => b.classList.toggle('active', b.dataset.sec === name));
  if (scroll) content.scrollTop = 0;
  document.getElementById('cpu-ctx-menu').classList.remove('open');
  fitPage();
}
navItems.forEach(btn => btn.addEventListener('click', () => showTab(btn.dataset.sec)));
showTab('overview', { scroll: false });
fitPage();

/* ── generic sortable/filterable process table ──────────────────────────── */
class ProcTable {
  constructor(wrapSel, columns, opts) {
    this.wrap = document.querySelector(wrapSel);
    this.columns = columns;
    this.opts = opts;
    this.sort = { key: 'cpu', dir: -1 };
    this.rows = new Map();            // pid -> {tr, tds}
    this.groupRows = new Map();       // title -> tr
    this.selectedPid = null;
    this.search = '';
    this.lastItems = null;
    this.buildShell();
  }

  buildShell() {
    const t = document.createElement('table');
    t.className = 'ptable';
    const trh = document.createElement('tr');
    for (const c of this.columns) {
      const th = document.createElement('th');
      th.textContent = c.label;
      if (c.num) th.classList.add('num');
      if (c.width) th.style.width = c.width;
      th.dataset.key = c.key;
      th.addEventListener('click', () => {
        this.sort = { key: c.key, dir: this.sort.key === c.key ? -this.sort.dir : (c.num ? -1 : 1) };
        if (this.lastItems) this.render(this.lastItems);
      });
      trh.appendChild(th);
    }
    const thead = document.createElement('thead');
    thead.appendChild(trh);
    this.tbody = document.createElement('tbody');
    t.append(thead, this.tbody);
    this.wrap.appendChild(t);
  }

  render(items) {
    this.lastItems = items;
    let list = items;
    if (this.search) {
      const q = this.search.toLowerCase();
      list = items.filter(p => this.opts.searchKeys(p).some(s => String(s).toLowerCase().includes(q)));
    }
    const { key, dir } = this.sort;
    const val = p => (key === 'name' || key === 'user' || key === 'state') ? String(p[key] ?? '').toLowerCase() : (p[key] ?? -1);
    list = [...list].sort((a, b) => {
      const va = val(a), vb = val(b);
      if (va === vb) return (a.pid - b.pid);
      return (va > vb ? 1 : -1) * dir;
    });
    const groups = this.opts.groups ? this.opts.groups(list) : [{ title: null, items: list }];
    const frag = document.createDocumentFragment();
    for (const g of groups) {
      if (g.title != null) {
        let gr = this.groupRows.get(g.title);
        if (!gr) { gr = document.createElement('tr'); gr.className = 'group-head'; gr.innerHTML = `<td colspan="${this.columns.length}"></td>`; this.groupRows.set(g.title, gr); }
        gr.querySelector('td').innerHTML = `<b>${esc(g.title)}</b> <span class="v-dim">(${g.items.length})</span>`;
        frag.appendChild(gr);
      }
      for (const p of g.items) frag.appendChild(this.rowEl(p));
    }
    this.tbody.replaceChildren(frag);
    this.wrap.querySelectorAll('th').forEach(th => {
      th.classList.toggle('th-sort', th.dataset.key === this.sort.key);
      th.classList.toggle('asc', th.dataset.key === this.sort.key && this.sort.dir === 1);
    });
  }

  rowEl(p) {
    let r = this.rows.get(p.pid);
    if (!r) {
      const tr = document.createElement('tr');
      tr.dataset.pid = String(p.pid);
      const tds = this.columns.map(() => tr.appendChild(document.createElement('td')));
      tr.addEventListener('click', () => this.select(p.pid));
      r = { tr, tds };
      this.rows.set(p.pid, r);
    }
    r.tr.className = 'prow' + (this.opts.rowCls ? ' ' + this.opts.rowCls(p) : '') + (this.selectedPid === p.pid ? ' selected' : '');
    const cells = this.opts.cells(p);
    this.columns.forEach((c, i) => {
      const cell = cells[i];
      const td = r.tds[i];
      td.className = (c.num ? 'num ' : '') + (cell.cls ? cell.cls : '');
      if (cell.title != null) td.title = cell.title; else td.removeAttribute('title');
      if (cell.html != null) { if (td._h !== cell.html) { td.innerHTML = cell.html; td._h = cell.html; } }
      else { const s = cell.text ?? ''; if (td.textContent !== s) td.textContent = s; }
    });
    return r.tr;
  }

  select(pid) {
    this.selectedPid = pid === this.selectedPid ? null : pid;
    for (const [p, r] of this.rows) r.tr.classList.toggle('selected', p === this.selectedPid);
    this.opts.onSelect?.(this.selectedPid);
  }
}

/* ── running tasks table ────────────────────────────────────────────────── */
let lastPerf = null, lastProcs = null, selfUid = null;

const tasksTable = new ProcTable('#tasks-table',
  [
    { key: 'name',   label: 'Name',    width: '38%' },
    { key: 'state',  label: 'Status',  width: '10%' },
    { key: 'cpu',    label: 'CPU',     num: true, width: '9%' },
    { key: 'diskBs', label: 'Disk',    num: true, width: '12%' },
    { key: 'netBs',  label: 'Network', num: true, width: '12%' },
    { key: 'mem',    label: 'Memory',  num: true, width: '15%' },
  ],
  {
    rowCls: p => p.isSelf ? 'self' : '',
    searchKeys: p => [p.name, p.cmdline, p.pid],
    groups: procsGroups,
    onSelect: pid => { btnEnd.disabled = pid == null; },
    cells: p => [
      { html: `<span class="pname" title="${esc(p.cmdline || p.name)} · ${esc(p.user || '')}">${esc(p.name)}</span>` },
      { text: STATE_NAMES[p.state] || '—', cls: 'state-badge' },
      { text: fPct(p.cpu), cls: vCls(p.cpu) },
      { text: p.diskBs == null ? '—' : fBytes(p.diskBs, true), cls: p.diskBs ? '' : 'v-dim' },
      { text: '—', cls: 'v-dim' },
      { text: fBytes(p.mem) },
    ],
  });

function procsGroups(list) {
  const apps = list.filter(p => p.isApp);
  const own  = list.filter(p => !p.isApp && (selfUid == null || p.uid === selfUid));
  const oth  = list.filter(p => !p.isApp && selfUid != null && p.uid !== selfUid);
  const out = [];
  if (apps.length) out.push({ title: 'Apps', items: apps });
  if (own.length)  out.push({ title: 'Background processes', items: own });
  if (oth.length)  out.push({ title: 'Other users & system', items: oth });
  if (!out.length) out.push({ title: null, items: list });
  return out;
}

const btnEnd = document.getElementById('btn-end-task');
btnEnd.onclick = () => requestKill();
document.getElementById('task-search').addEventListener('input', e => {
  tasksTable.search = e.target.value.trim();
  if (lastProcs) tasksTable.render(lastProcs.procs);
});

/* ── kill modal ─────────────────────────────────────────────────────────── */
const modal = document.getElementById('modal');
let killTarget = null;
function requestKill() {
  const p = lastProcs?.procs.find(x => x.pid === tasksTable.selectedPid);
  if (!p) return;
  killTarget = p.pid;
  document.getElementById('modal-text').textContent = `End “${p.name}” (PID ${p.pid})?`;
  modal.hidden = false;
}
document.getElementById('modal-cancel').onclick = () => { modal.hidden = true; };
modal.addEventListener('click', e => { if (e.target === modal) modal.hidden = true; });
document.getElementById('modal-ok').onclick = () => doKill(false);
document.getElementById('modal-force').onclick = () => doKill(true);
async function doKill(force) {
  modal.hidden = true;
  if (killTarget == null) return;
  const r = await window.api.killProcess(killTarget, force);
  if (!r.ok && r.error && r.error !== 'ESRCH') {
    btnEnd.textContent = 'Access denied';
    setTimeout(() => { btnEnd.textContent = 'End task'; }, 1200);
  }
  killTarget = null;
  procTick();
}

/* ── CPU section ────────────────────────────────────────────────────────── */
let cpuGraph = null, coreGraphs = [], coreValEls = [], coreGraphsOpen = false;

function renderCpu(p) {
  const cpu = p.cpu;
  const big = document.getElementById('cpu-big');
  const bigTxt = Math.round(cpu.usage) + '<small>%</small>';
  if (big._h !== bigTxt) { big.innerHTML = bigTxt; big._h = bigTxt; }
  const sub = document.getElementById('cpu-sub');
  if (sub._v !== cpu.model) { sub.textContent = cpu.model; sub._v = cpu.model; }

  if (!cpuGraph) {
    cpuGraph = makeGraph('cpu', document.getElementById('cpu-canvas'), {
      series: [{ color: '#4cc2ff', data: hist.cpuTotal, fill: .28 }],
      mode: 'percent', fmt: v => v.toFixed(0) + '%',
    });
  }

  const items = [
    ['Utilization', fPct(cpu.usage)],
    ['Speed', cpu.freqGHz ? cpu.freqGHz.toFixed(2) + ' GHz' : '—'],
    ['Up time', fUp(cpu.uptimeSec)],
    ['Logical processors', cpu.logical],
    ['Physical processors', cpu.physical],
    ['Base speed', cpu.baseFreq ? (cpu.baseFreq / 1000).toFixed(2) + ' GHz' : '—'],
    ['Max speed', cpu.maxFreq ? (cpu.maxFreq / 1000).toFixed(2) + ' GHz' : '—'],
    ['Load average', cpu.load ? cpu.load.map(x => x.toFixed(2)).join(' · ') : '—'],
  ];
  const statsEl = document.getElementById('cpu-stats');
  const vals = statsEl.querySelectorAll('.s-value');
  if (vals.length !== items.length) statsEl.innerHTML = statsRowHtml(items);
  else items.forEach(([l, v], i) => { const s = String(v ?? '—'); if (vals[i]._v !== s) { vals[i].innerHTML = s; vals[i]._v = s; } });

  if (coreGraphsOpen) {
    coreValEls.forEach(({ el, i }) => { const v = Math.round(cpu.perCore?.[i] ?? 0) + '%'; if (el.textContent !== v) el.textContent = v; });
  }
  // cores were requested before the first data tick — build them now
  if (coreGraphsOpen && !coreGraphs.length && (cpu.perCore?.length)) setCoreGraphs(true);
}

/* per-core graphs: right-click the CPU graph → "Show logical cores" →
   the grid replaces the main CPU graph in the same window */
function setCoreGraphs(open) {
  coreGraphsOpen = open;
  document.getElementById('sec-cpu').classList.toggle('cores-on', open);
  const host = document.getElementById('cores-host');
  if (!open) {
    destroyGraphs('core:');
    coreGraphs = []; coreValEls = [];
    host.innerHTML = '';
    fitPage();
    return;
  }
  const grid = document.createElement('div');
  grid.className = 'cores-grid';
  const n = lastPerf?.cpu.perCore?.length || 0;
  for (let i = 0; i < n; i++) {
    const cell = document.createElement('div');
    cell.className = 'core-cell';
    cell.innerHTML = `<div class="cc-head"><span>Core <b>${i}</b></span><b class="cc-val">${Math.round(lastPerf.cpu.perCore[i] || 0)}%</b></div><canvas></canvas>`;
    grid.appendChild(cell);
    coreValEls.push({ el: cell.querySelector('.cc-val'), i });
    coreGraphs.push(makeGraph(`core:${i}`, cell.querySelector('canvas'), {
      series: [{ color: '#4cc2ff', data: hist.cpuCores[i] || [], fill: .3 }],
      mode: 'percent', hover: false, fmt: v => v.toFixed(0) + '%',
    }));
  }
  host.innerHTML = '';
  host.appendChild(grid);
  fitPage();
}

const ctxMenu = document.getElementById('cpu-ctx-menu');
function closeCtx() { ctxMenu.classList.remove('open'); }
document.getElementById('cpu-graph-box').addEventListener('contextmenu', (e) => {
  e.preventDefault();
  e.stopPropagation(); // don't let the document-level closer eat this event
  ctxMenu.innerHTML = '';
  const item = document.createElement('div');
  item.className = 'ctx-item';
  item.innerHTML = `<span class="chk">${coreGraphsOpen ? '✓' : ''}</span><span>${coreGraphsOpen ? 'Hide logical cores' : 'Show logical cores'}</span>`;
  item.addEventListener('click', () => { setCoreGraphs(!coreGraphsOpen); closeCtx(); });
  ctxMenu.appendChild(item);
  ctxMenu.classList.add('open');
  const r = ctxMenu.getBoundingClientRect();
  const x = Math.max(6, Math.min(e.clientX, innerWidth - r.width - 6));
  const y = Math.max(6, Math.min(e.clientY, innerHeight - r.height - 6));
  ctxMenu.style.left = x + 'px';
  ctxMenu.style.top = y + 'px';
});
document.addEventListener('click', (e) => { if (!ctxMenu.contains(e.target)) closeCtx(); });
document.addEventListener('contextmenu', (e) => { if (!ctxMenu.contains(e.target)) closeCtx(); });
window.addEventListener('blur', closeCtx);
window.addEventListener('resize', closeCtx);
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeCtx(); });

/* ── Memory section ─────────────────────────────────────────────────────── */
let memGraph = null;
function renderMem(p) {
  const m = p.memory;
  const pct = m.total ? 100 * m.inUse / m.total : 0;
  const big = document.getElementById('mem-big');
  const bigTxt = Math.round(pct) + '<small>%</small>';
  if (big._h !== bigTxt) { big.innerHTML = bigTxt; big._h = bigTxt; }
  const sub = document.getElementById('mem-sub');
  const subTxt = `${fBytes(m.inUse)} of ${fBytes(m.total)}`;
  if (sub._v !== subTxt) { sub.textContent = subTxt; sub._v = subTxt; }

  if (!memGraph) {
    memGraph = makeGraph('mem', document.getElementById('mem-canvas'), {
      series: [{ color: '#b08cff', data: hist.mem, fill: .28 }],
      mode: 'percent', fmt: v => v.toFixed(0) + '%',
    });
  }
  const items = [
    ['In use', fBytes(m.inUse)],
    ['Available', fBytes(m.available)],
    ['Total', fBytes(m.total)],
    ['Cached', fBytes(m.cached)],
    ['Swap used', m.swapTotal ? `${fBytes(m.swapUsed)} / ${fBytes(m.swapTotal)}` : 'none'],
    ['Committed', m.committed && m.commitLimit ? `${fBytes(m.committed)} / ${fBytes(m.commitLimit)}` : '—'],
  ];
  const statsEl = document.getElementById('mem-stats');
  const vals = statsEl.querySelectorAll('.s-value');
  if (vals.length !== items.length) statsEl.innerHTML = statsRowHtml(items);
  else items.forEach(([l, v], i) => { const s = String(v ?? '—'); if (vals[i]._v !== s) { vals[i].innerHTML = s; vals[i]._v = s; } });
}

/* ── sub-block sections (GPU / Disks / Network) ─────────────────────────── */
// Each section: { host, structKey(), block(key,data) → {el, refs, graphs[]} }
const blockSections = {
  gpu:  { key: 'gpu',  host: document.getElementById('gpu-blocks'),  last: null, blocks: new Map() },
  disk: { key: 'disk', host: document.getElementById('disk-blocks'), last: null, blocks: new Map() },
  net:  { key: 'net',  host: document.getElementById('net-blocks'),  last: null, blocks: new Map() },
};

function gpuStats(g) {
  return [
    ['Dedicated memory', g.memUsed != null ? `${fBytes(g.memUsed)} / ${fBytes(g.memTotal)}` : '—'],
    ['Temperature', g.temp != null ? g.temp + ' °C' : '—'],
    ['Power', g.powerW != null ? `${g.powerW} W${g.powerLimitW ? ' / ' + g.powerLimitW + ' W' : ''}` : '—'],
    ['Fan', g.fanPct != null ? g.fanPct + '%' : '—'],
    ['GPU clock', g.clockGpuMHz != null ? g.clockGpuMHz + ' MHz' : '—'],
    ['Memory clock', g.clockMemMHz != null ? g.clockMemMHz + ' MHz' : '—'],
    ['Driver', esc(g.driver || '—')],
    ['Vendor', esc(g.vendor || '—')],
  ];
}
function diskStats(d) {
  return [
    ['Active time', fPct(d.active)],
    ['Read speed', fBytes(d.readBs, true)],
    ['Write speed', fBytes(d.writeBs, true)],
    ['Capacity', fBytes(d.size)],
    ['Disk space', d.usage ? `${Math.round(d.usage.usedPct)}% used <small>(${esc(d.usage.mount)})</small>` : '—'],
    ['Type', d.type || '—'],
    ['Model', esc(d.model || d.id)],
  ];
}
function netStats(n) {
  return [
    ['Receive', fBytes(n.rxBs, true)],
    ['Send', fBytes(n.txBs, true)],
    ['Link speed', n.speedMbps ? (n.speedMbps >= 1000 ? (n.speedMbps / 1000) + ' Gbps' : n.speedMbps + ' Mbps') : '—'],
    ['IPv4', n.ip || '—'],
    ['MAC', n.mac || '—'],
    ['State', esc(n.operstate || '—')],
  ];
}

function makeBlockShell(title, sub, extra) {
  const el = document.createElement('div');
  el.className = 'subblock';
  el.innerHTML = `
    <div class="sb-head">
      <span class="sb-title">${esc(title)}</span>
      <span class="sb-sub">${esc(sub || '')}</span>
      ${extra || ''}
      <span class="sb-big">—</span>
    </div>
    <div class="graph-box"><canvas></canvas><div class="graph-hint"></div></div>
    <div class="stats-row"></div>`;
  return {
    el,
    big: el.querySelector('.sb-big'),
    sub: el.querySelector('.sb-sub'),
    hint: el.querySelector('.graph-hint'),
    stats: el.querySelector('.stats-row'),
    canvas: el.querySelector('canvas'),
  };
}

function updateStatsRow(row, items) {
  const vals = row.querySelectorAll('.s-value');
  if (vals.length !== items.length) { row.innerHTML = statsRowHtml(items); return; }
  items.forEach(([l, v], i) => { const s = String(v ?? '—'); if (vals[i]._v !== s) { vals[i].innerHTML = s; vals[i]._v = s; } });
}

function rebuildSection(sec, entries, builders) {
  const { host, blocks } = sec;
  destroyGraphs(sec.key + ':');
  blocks.clear();
  host.innerHTML = '';
  for (const [key, data] of entries) {
    const b = builders(key, data);
    blocks.set(key, b);
    host.appendChild(b.shell.el);
  }
}

function renderGpuSection(p) {
  const sec = blockSections.gpu;
  const gpus = p.gpus;
  const key = gpus.map(g => g.id + (g.telemetry ? 1 : 0)).join(',');
  document.getElementById('gpu-sub').textContent = gpus.length + (gpus.length === 1 ? ' adapter' : ' adapters');
  if (sec.last !== key) {
    sec.last = key;
    rebuildSection(sec, gpus.map((g, i) => [`gpu:${g.id}`, { g, i }]), (bk, { g, i }) => {
      const extra = g.telemetry ? '' : '<span class="sb-badge">no live telemetry</span>';
      const shell = makeBlockShell(`GPU ${i + 1}`, g.model, extra);
      shell.big.style.color = '#5ce08a';
      const graphs = [];
      if (g.telemetry) {
        shell.hint.textContent = 'Utilization (%)';
        graphs.push(makeGraph(`gpu:${bk}`, shell.canvas, {
          series: [{ color: '#5ce08a', data: (hist.gpus.get(g.id) || { util: [] }).util, fill: .25 }],
          mode: 'percent', fmt: v => v.toFixed(0) + '%',
        }));
      } else {
        shell.hint.textContent = '';
        shell.canvas.style.visibility = 'hidden';
        const note = document.createElement('div');
        note.className = 'no-telemetry';
        note.innerHTML = `<div><b>${esc(g.model || 'GPU')} detected</b> — live telemetry unavailable.</div>
          <div>Driver ${esc(g.driver || '?')} loaded; NVML unreachable (missing <code>/dev/nvidia*</code> nodes).</div>
          <div>Fix with <code>sudo nvidia-modprobe -u -c 0 -c 1</code> (one <code>-c</code> per GPU) — usage appears automatically.</div>`;
        shell.el.appendChild(note);
      }
      shell.stats.innerHTML = statsRowHtml(gpuStats(g));
      return { shell, graphs };
    });
  }
  gpus.forEach((g, i) => {
    const b = sec.blocks.get(`gpu:${g.id}`);
    if (!b) return;
    const bigTxt = g.telemetry ? Math.round(g.util ?? 0) + '<small>%</small>' : '<span class="v-dim">—</span>';
    if (b.shell.big._h !== bigTxt) { b.shell.big.innerHTML = bigTxt; b.shell.big._h = bigTxt; }
    updateStatsRow(b.shell.stats, gpuStats(g));
  });
}

function renderDiskSection(p) {
  const sec = blockSections.disk;
  const disks = p.disks;
  const key = disks.map(d => d.id).join(',');
  document.getElementById('disk-sub').textContent = disks.length + (disks.length === 1 ? ' disk' : ' disks');
  if (sec.last !== key) {
    sec.last = key;
    rebuildSection(sec, disks.map(d => [`disk:${d.id}`, d]), (bk, d) => {
      const mount = d.usage && d.usage.mount;
      const extra = mount ? '<span class="sb-badge browse-hint">Browse</span>' : '';
      const shell = makeBlockShell(d.id, [d.type, d.model].filter(Boolean).join(' · '), extra);
      shell.big.style.color = '#ffc860';
      shell.hint.textContent = 'Active time (%)';
      const graphs = [makeGraph(`disk:${bk}`, shell.canvas, {
        series: [{ color: '#ffc860', data: (hist.disks.get(d.id) || { active: [] }).active, fill: .25 }],
        mode: 'percent', fmt: v => v.toFixed(0) + '%',
      })];
      shell.stats.innerHTML = statsRowHtml(diskStats(d));
      if (mount) {
        shell.el.classList.add('disk-browse');
        shell.el.title = `Browse ${d.id} space usage`;
        shell.el.addEventListener('click', () => diskgrid.open(d));
      }
      return { shell, graphs };
    });
  }
  disks.forEach(d => {
    const b = sec.blocks.get(`disk:${d.id}`);
    if (!b) return;
    const bigTxt = Math.round(d.active) + '<small>%</small>';
    if (b.shell.big._h !== bigTxt) { b.shell.big.innerHTML = bigTxt; b.shell.big._h = bigTxt; }
    updateStatsRow(b.shell.stats, diskStats(d));
  });
}

/* ── disk space grid — click a disk card → treemap of its biggest files ── */
// The scan runs once per mount in the background (du-style, `disk_scan_*`
// commands). The landing view is a FLAT grid of the largest individual files
// across the whole mount (area ∝ size), so the space hogs are visible without
// clicking through folders; right-click a file to jump to its containing
// folder, which opens the classic per-directory drill-down.
const diskgrid = (() => {
  const TOP_FILES = 250; // tiles in the flat view (rest collapses to one tile)
  const secEl = document.getElementById('sec-disk');
  const bar = document.getElementById('diskgrid-bar');
  const backBtn = document.getElementById('diskgrid-back');
  const crumbEl = document.getElementById('diskgrid-crumb');
  const progEl = document.getElementById('diskgrid-progress');
  const closeBtn = document.getElementById('diskgrid-close');
  const host = document.getElementById('disk-grid');
  const cv = document.getElementById('diskgrid-canvas');
  const tip = document.getElementById('diskgrid-tip');
  const emptyEl = document.getElementById('diskgrid-empty');
  const ctxEl = document.getElementById('diskgrid-ctx');
  let grid = null;      // TreemapGrid instance (lazy — after first open)
  let stack = [];       // [flatNode, ...folder nodes] — path from the root view
  let rootNode = null;  // full scanned tree (for resolving file paths)
  let flatNode = null;  // pseudo-node: largest files across the whole mount
  let rootPath = '';
  let scanning = false;
  let gen = 0;          // generation counter — stale polls stop themselves
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  function ensureGrid() {
    if (!grid) grid = new TreemapGrid(cv, tip, { onOpen: openNode, onContext: showCtx });
    return grid;
  }

  const cur = () => stack[stack.length - 1];

  // Walk the whole scanned tree and collect every file with its path relative
  // to the mount, then keep the TOP_FILES largest. Files dropped by the
  // backend's per-directory cap are invisible here, but those are by
  // definition smaller than the 250 kept in their directory; `totalFiles`
  // (the scan-wide counter) keeps the "… N more" label honest.
  function buildFlat(root, totalFiles) {
    const files = [];
    const walk = [{ node: root, prefix: '' }];
    while (walk.length) {
      const { node, prefix } = walk.pop();
      for (const c of node.children || []) {
        if (c.kind === 'file') files.push({ name: c.name, size: c.size, path: prefix + c.name });
        else if (c.children && c.children.length) walk.push({ node: c, prefix: prefix + c.name + '/' });
      }
    }
    files.sort((a, b) => b.size - a.size);
    const shown = files.slice(0, TOP_FILES);
    const shownBytes = shown.reduce((s, f) => s + f.size, 0);
    return {
      name: 'Top files', flat: true, size: root.size,
      files: shown,
      restCount: Math.max(0, (totalFiles || files.length) - shown.length),
      restSize: Math.max(0, root.size - shownBytes),
    };
  }

  // The largest kept children plus a display-only "… N more" bucket holding
  // the bytes of entries dropped by the per-directory cap.
  function entriesFor(node) {
    let list;
    if (node.flat) {
      list = node.files.map((f) => ({ name: f.name, kind: 'file', size: f.size, fmtSize: fBytes(f.size), path: f.path }));
      if (node.restCount > 0 && node.restSize > 0) {
        list.push({ name: `… ${node.restCount.toLocaleString()} more files`, kind: 'other', size: node.restSize, fmtSize: fBytes(node.restSize) });
      }
    } else {
      list = node.children.map((c) => ({ ...c, fmtSize: fBytes(c.size) }));
      if (node.omitted > 0 && node.hidden > 0) {
        list.push({ name: `… ${node.omitted} more`, kind: 'other', size: node.hidden, fmtSize: fBytes(node.hidden) });
      }
    }
    list.sort((a, b) => b.size - a.size);
    return list;
  }

  function render() {
    const node = cur();
    if (!node) return;
    emptyEl.hidden = true;
    ensureGrid().setData(entriesFor(node));
    renderCrumb();
    backBtn.disabled = stack.length <= 1;
  }

  function renderCrumb() {
    crumbEl.innerHTML = '';
    const add = (text, title, onClick, isCur) => {
      const b = document.createElement('button');
      b.className = 'crumb-link' + (isCur ? ' cur' : '');
      b.textContent = text;
      b.title = title || text;
      if (onClick) b.addEventListener('click', onClick);
      crumbEl.appendChild(b);
    };
    const sep = () => {
      const s = document.createElement('span');
      s.className = 'crumb-sep';
      s.textContent = '›';
      crumbEl.appendChild(s);
    };
    // stack[0] is the flat "Top files" view, labelled with the mount path.
    stack.forEach((n, i) => {
      if (i) sep();
      const isCur = i === stack.length - 1;
      const label = n.flat ? (rootPath || '/') : n.name;
      add(label, n.flat ? `Largest files on ${rootPath}` : label,
        !isCur ? () => { stack = stack.slice(0, i + 1); render(); } : null, isCur);
    });
  }

  function openNode(node) {
    stack.push(node);
    render();
  }

  // Right-click menu on a tile. Files offer the jump to their containing
  // folder (the whole point of the flat view); directories offer Open.
  function closeCtx() { ctxEl.classList.remove('open'); }
  function showCtx(e, ev) {
    const items = [];
    if (e.kind === 'file' && e.path) {
      const dir = e.path.includes('/') ? e.path.slice(0, e.path.lastIndexOf('/')) : '';
      items.push({
        label: dir ? `Browse to ${rootPath}/${dir}` : `Browse to ${rootPath}`,
        fn: () => browseToFolder(e.path),
      });
    } else if (e.kind === 'dir') {
      items.push({ label: `Open ${e.name}`, fn: () => openNode(e) });
    }
    if (!items.length) return;
    ctxEl.innerHTML = '';
    for (const it of items) {
      const d = document.createElement('div');
      d.className = 'ctx-item';
      d.textContent = it.label;
      d.addEventListener('click', () => { closeCtx(); it.fn(); });
      ctxEl.appendChild(d);
    }
    ctxEl.classList.add('open');
    const r = ctxEl.getBoundingClientRect();
    ctxEl.style.left = Math.max(6, Math.min(ev.clientX, innerWidth - r.width - 6)) + 'px';
    ctxEl.style.top = Math.max(6, Math.min(ev.clientY, innerHeight - r.height - 6)) + 'px';
  }
  document.addEventListener('click', (e) => { if (!ctxEl.contains(e.target)) closeCtx(); });
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeCtx(); });
  window.addEventListener('blur', closeCtx);

  // Resolve a file's relative path against the scanned tree and switch the
  // grid to the classic folder drill-down at its parent directory.
  function browseToFolder(relPath) {
    if (!rootNode) return false;
    const segs = relPath.split('/').slice(0, -1);
    let node = rootNode;
    const chain = [];
    for (const s of segs) {
      const c = (node.children || []).find((x) => x.kind === 'dir' && x.name === s);
      if (!c) return false;
      chain.push(c);
      node = c;
    }
    stack = [flatNode, ...chain];
    render();
    return true;
  }

  function showEmpty(html) {
    emptyEl.innerHTML = html;
    emptyEl.hidden = false;
  }

  // The backend runs one scan at a time; if another is still winding down,
  // cancel it and retry.
  async function startScan(path, g) {
    for (let i = 0; i < 60; i++) {
      if (g !== gen) return false;
      const r = await api.diskScanStart(path);
      if (r.ok) return true;
      if (!/already running/.test(r.error || '')) {
        showEmpty(`<div><b>Scan failed</b></div><div>${esc(r.error)}</div>`);
        return false;
      }
      await api.diskScanCancel();
      await sleep(200);
    }
    showEmpty('<div><b>Could not start the scan</b></div>');
    return false;
  }

  async function poll(g) {
    while (g === gen) {
      const s = await api.diskScanStatus();
      if (g !== gen) return;
      if (s.done) {
        scanning = false;
        progEl.hidden = true;
        if (s.cancelled) { showEmpty('<div><b>Scan cancelled</b></div>'); return; }
        if (s.error) { showEmpty(`<div><b>Scan failed</b></div><div>${esc(s.error)}</div>`); return; }
        if (!s.tree || !s.tree.children) { showEmpty('<div><b>Nothing to show</b></div>'); return; }
        rootNode = s.tree;
        flatNode = buildFlat(rootNode, s.files);
        stack = [flatNode];
        render();
        return;
      }
      scanning = true;
      emptyEl.hidden = false;
      showEmpty(`<div><b>Scanning ${esc(s.path)}</b></div>
        <div class="muted">${s.files.toLocaleString()} files · ${fBytes(s.bytes)} · ${Math.round(s.elapsedMs / 1000)}s</div>
        <div class="muted">${esc(s.current || '')}</div>`);
      progEl.hidden = false;
      progEl.textContent = `${s.files.toLocaleString()} files · ${fBytes(s.bytes)} · ${Math.round(s.elapsedMs / 1000)}s` +
        (s.errors ? ` · ${s.errors} unreadable` : '');
      await sleep(300);
    }
  }

  async function open(disk) {
    gen++;
    const g = gen;
    stack = [];
    rootNode = null;
    flatNode = null;
    closeCtx();
    scanning = false;
    rootPath = disk.usage && disk.usage.mount ? disk.usage.mount : '';
    secEl.classList.add('grid-on');
    bar.hidden = false;
    host.hidden = false;
    crumbEl.innerHTML = '';
    backBtn.disabled = true;
    progEl.hidden = true;
    ensureGrid().clear();
    if (!rootPath) {
      showEmpty(`<div><b>${esc(disk.model || disk.type || disk.id)}</b> has no mount point.</div>
        <div class="muted">Nothing to browse — the disk may be unformatted or offline.</div>`);
      return;
    }
    showEmpty(`<div><b>${esc(rootPath)}</b></div><div class="muted">Starting scan…</div>`);
    if (await startScan(rootPath, g)) poll(g);
  }

  function close() {
    gen++;
    closeCtx();
    if (scanning) api.diskScanCancel().catch(() => {});
    scanning = false;
    secEl.classList.remove('grid-on');
    host.hidden = true;
    bar.hidden = true;
    progEl.hidden = true;
    if (grid) grid.clear();
  }

  backBtn.addEventListener('click', () => {
    if (stack.length > 1) { stack.pop(); render(); }
  });
  closeBtn.addEventListener('click', close);
  window.addEventListener('resize', () => { if (grid && !host.hidden) grid.draw(); });

  return {
    open, close, isOpen: () => !host.hidden,
    // verification helpers
    _testFlat: () => (cur() && cur().flat ? {
      files: cur().files.length,
      top: cur().files[0] ? cur().files[0].name : null,
      restCount: cur().restCount,
      restSize: cur().restSize,
    } : null),
    // drill down without a synthetic click: from the flat view, find a file by
    // name and browse to its containing folder; from a folder view, open the
    // named subdirectory.
    _testDrill: (name) => {
      const n = stack[stack.length - 1];
      if (!n) return false;
      if (n.flat) {
        const f = n.files.find((x) => x.name === name);
        return f ? browseToFolder(f.path) : false;
      }
      const c = n.children.find((x) => x.name === name && x.kind === 'dir');
      if (!c) return false;
      openNode(c);
      return true;
    },
  };
})();

// exposed for verify modes / debugging
window.__diskgrid = diskgrid;

function renderNetSection(p) {
  const sec = blockSections.net;
  const nets = p.nets;
  const key = nets.map(n => n.id).join(',');
  document.getElementById('net-sub').textContent = nets.length + (nets.length === 1 ? ' adapter' : ' adapters');
  if (sec.last !== key) {
    sec.last = key;
    rebuildSection(sec, nets.map(n => [`net:${n.id}`, n]), (bk, n) => {
      const title = /^(wl|wlan)/i.test(n.name) ? 'Wi-Fi' : /^(en|eth)/i.test(n.name) ? 'Ethernet' : n.name;
      const shell = makeBlockShell(title, n.name + (n.ip ? ` · ${n.ip}` : ''));
      shell.hint.textContent = 'Throughput';
      const legend = document.createElement('div');
      legend.className = 'legend';
      legend.innerHTML = '<span><i style="background:#4cc2ff"></i>Receive</span><span><i style="background:#ff8f8f"></i>Send</span>';
      shell.el.insertBefore(legend, shell.stats);
      const h = hist.nets.get(n.id) || { rx: [], tx: [] };
      const graphs = [makeGraph(`net:${bk}`, shell.canvas, {
        series: [{ name: 'RX', color: '#4cc2ff', data: h.rx, fill: .2 }, { name: 'TX', color: '#ff8f8f', data: h.tx, fill: .1 }],
        mode: 'auto', fmt: v => fBytes(v, true),
      })];
      shell.stats.innerHTML = statsRowHtml(netStats(n));
      return { shell, graphs };
    });
  }
  nets.forEach(n => {
    const b = sec.blocks.get(`net:${n.id}`);
    if (!b) return;
    const bigTxt = `${fBytes(n.rxBs, true)}<small>↓ ${fBytes(n.txBs, true)} ↑</small>`;
    if (b.shell.big._h !== bigTxt) { b.shell.big.innerHTML = bigTxt; b.shell.big._h = bigTxt; }
    updateStatsRow(b.shell.stats, netStats(n));
  });
}

const statsRowHtml = items => `<div style="display:contents">${items.map(([l, v]) =>
  `<div class="stat"><div class="s-label">${l}</div><div class="s-value">${v == null ? '—' : v}</div></div>`).join('')}</div>`;

/* ── activity log (record per-proc CPU/GPU, review + flag suspicious) ───── */
const logLive = document.getElementById('log-live');
const logReview = document.getElementById('log-review');
const btnLogStart = document.getElementById('btn-log-start');
const btnLogStop = document.getElementById('btn-log-stop');
const logStatusEl = document.getElementById('log-status');
const logRecdot = document.getElementById('log-recdot');
const logHeroTitle = document.getElementById('log-hero-title');
const logHeroSub = document.getElementById('log-hero-sub');
const logLiveHint = document.getElementById('log-live-hint');
let logState = 'idle';            // idle | recording | review
let logStatusTimer = null;
let logGraph = null;

const fElapsed = ms => {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  return m ? `${m}:${String(s % 60).padStart(2, '0')}` : `${s}s`;
};

function setLogState(state) {
  logState = state;
  const recording = state === 'recording';
  const review = state === 'review';
  logLive.hidden = review;
  logReview.hidden = !review;
  btnLogStart.disabled = recording;
  btnLogStop.disabled = !recording;
  logRecdot.hidden = !recording;
  if (state === 'idle') {
    logHeroTitle.textContent = 'Ready to record';
    logHeroSub.textContent = 'Capture which processes use the CPU and GPU over time, then review the activity — with a flag list that calls out anything doing more than it should (GPU use, sustained CPU, spikes, heavy VRAM, headless hogs).';
    logStatusEl.textContent = '';
  } else if (recording) {
    logHeroTitle.textContent = 'Recording…';
    logHeroSub.textContent = 'Sampling every process and GPU client once per second. Keep using the app — stop whenever you like.';
    logStatusEl.textContent = '0s';
  }
  fitPage();
}

async function onLogStart() {
  try {
    const r = await window.api.logStart();
    if (!r.ok) { logStatusEl.textContent = r.error || 'could not start'; return; }
    setLogState('recording');
    logStatusTimer = setInterval(async () => {
      try {
        const s = await window.api.logStatus();
        if (s.active) {
          logStatusEl.textContent = fElapsed(s.elapsedMs) + ' · ' + s.nTicks + ' ticks';
          logHeroTitle.textContent = 'Recording… ' + fElapsed(s.elapsedMs);
        } else {
          // stopped elsewhere — refresh review
          clearInterval(logStatusTimer); logStatusTimer = null;
          onLogStop();
        }
      } catch (e) { /* ignore */ }
    }, 1000);
  } catch (e) { console.error('logStart', e); }
}

async function onLogStop() {
  clearInterval(logStatusTimer); logStatusTimer = null;
  let data;
  try { data = await window.api.logStop(); }
  catch (e) { console.error('logStop', e); return; }
  renderLogReview(data);
  setLogState('review');
}

btnLogStart.onclick = onLogStart;
btnLogStop.onclick = onLogStop;

function renderLogReview(data) {
  const sub = document.getElementById('log-sub');
  if (data.empty) {
    sub.textContent = '';
    logLiveHint.hidden = false;
    logHeroTitle.textContent = 'Nothing captured';
    logHeroSub.textContent = data.note || 'Start a log and stop it after a while to see the review.';
    logLive.hidden = false; logReview.hidden = true;
    setLogState('idle');
    return;
  }
  logLiveHint.hidden = true;
  const dur = Math.round(data.durationMs / 1000);
  sub.textContent = `${fElapsed(data.durationMs)} · ${data.nTicks} samples · ${data.cpuCores} cores`;
  if (data.note) sub.textContent += '  ·  ' + data.note;

  // ── timeline graph ────────────────────────────────────────────────────
  const base = performance.now();
  const win = Math.max(data.durationMs, data.intervalMs);
  const toT = i => base - (win - i * data.intervalMs);
  const series = [];
  if (Array.isArray(data.totalSeries) && data.totalSeries.length) {
    series.push({ name: 'total', color: '#5a5a63', fill: .18, data: data.totalSeries.map(p => ({ t: toT(p.t / data.intervalMs), v: p.v })) });
  }
  for (const s of data.series || []) {
    series.push({ name: s.name, color: s.color, fill: .1, data: s.points.map(p => ({ t: toT(p.i), v: p.v })) });
  }
  if (logGraph) { logGraph._ro?.disconnect(); graphs.delete('log'); logGraph = null; }
  logGraph = new LineGraph(document.getElementById('log-canvas'), {
    series, mode: 'percent', fixedBase: base, windowMs: win,
    fmt: v => v.toFixed(1) + '%',
  });
  graphs.set('log', logGraph);
  logGraph.render();

  // legend (top apps that have a line)
  const legend = document.getElementById('log-legend');
  legend.innerHTML = (data.series || []).map(s =>
    `<span><i style="background:${s.color}"></i>${esc(s.name)}</span>`).join('') +
    '<span><i style="background:#5a5a63"></i>total</span>';
  document.getElementById('log-graph-sub').textContent =
    data.gpuProcsSupported ? '' : '· per-process GPU unavailable (NVML unreachable) — CPU only';

  // ── suspicious flags ──────────────────────────────────────────────────
  const flagsEl = document.getElementById('log-flags');
  const flags = data.flags || [];
  document.getElementById('log-flag-count').textContent = flags.length;
  if (!flags.length) {
    flagsEl.innerHTML = '<div class="log-flag-none muted">Nothing unusual — no process stood out during this window.</div>';
  } else {
    const sevLabel = { 2: ['high', 'sev-high'], 1: ['med', 'sev-med'], 0: ['low', 'sev-low'] };
    flagsEl.innerHTML = flags.map(f => {
      const [label, cls] = sevLabel[f.severity] || ['low', 'sev-low'];
      return `<div class="log-flag">
        <div class="lf-head"><span class="lf-name">${esc(f.name)}</span>
          <span class="lf-badge ${cls}">${label}</span></div>
        <div class="lf-meta muted">${esc(f.user)} · avg ${f.avgCpu}% · peak ${f.peakCpu}%${f.gpuPct ? ` · GPU ${f.gpuPct}%` : ''}</div>
        <ul class="lf-reasons">${f.reasons.map(r => `<li>${esc(r)}</li>`).join('')}</ul>
      </div>`;
    }).join('');
  }

  // ── process table ─────────────────────────────────────────────────────
  const apps = data.apps || [];
  document.getElementById('log-app-count').textContent = apps.length;
  const wrap = document.getElementById('log-table');
  const cols = [
    { key: 'name', label: 'Process', width: '26%' },
    { key: 'user', label: 'User', width: '14%' },
    { key: 'avgCpu', label: 'Avg CPU', num: true, width: '11%' },
    { key: 'peakCpu', label: 'Peak', num: true, width: '10%' },
    { key: 'activePct', label: 'Active', num: true, width: '10%' },
    { key: 'coreSec', label: 'CPU time', num: true, width: '11%' },
    { key: 'gpuPct', label: 'GPU', num: true, width: '9%' },
    { key: 'maxVramGb', label: 'VRAM', num: true, width: '9%' },
  ];
  let sort = { key: 'coreSec', dir: -1 };
  const build = () => {
    const list = [...apps].sort((a, b) => {
      const va = (sort.key === 'name' || sort.key === 'user') ? String(a[sort.key] ?? '') : (a[sort.key] ?? -1);
      const vb = (sort.key === 'name' || sort.key === 'user') ? String(b[sort.key] ?? '') : (b[sort.key] ?? -1);
      return (va > vb ? 1 : va < vb ? -1 : 0) * sort.dir;
    });
    const gpuOk = data.gpuProcsSupported;
    wrap.innerHTML = `<table class="ptable log-htable"><thead><tr>${cols.map(c =>
      `<th class="${c.num ? 'num' : ''}" data-key="${c.key}" style="width:${c.width}">${c.label}</th>`).join('')}</tr></thead>
      <tbody>${list.map(a => `<tr>
        <td><span class="pname">${esc(a.name)}</span>${a.isApp ? '<span class="log-appchip">app</span>' : ''}</td>
        <td class="v-dim">${esc(a.user)}</td>
        <td class="num ${vCls(a.avgCpu)}">${fPct(a.avgCpu)}</td>
        <td class="num ${vCls(a.peakCpu)}">${fPct(a.peakCpu)}</td>
        <td class="num">${a.activePct}%</td>
        <td class="num">${a.coreSec >= 10 ? Math.round(a.coreSec) : a.coreSec.toFixed(1)} s</td>
        <td class="num ${gpuOk ? '' : 'v-dim'}">${gpuOk ? (a.gpuPct ? a.gpuPct + '%' : '—') : '—'}</td>
        <td class="num ${gpuOk ? '' : 'v-dim'}">${gpuOk && a.maxVramGb ? a.maxVramGb + ' GB' : '—'}</td>
      </tr>`).join('')}</tbody></table>`;
    wrap.querySelectorAll('th').forEach(th => {
      th.classList.toggle('th-sort', th.dataset.key === sort.key);
      th.classList.toggle('asc', th.dataset.key === sort.key && sort.dir === 1);
      th.onclick = () => {
        sort = { key: th.dataset.key, dir: sort.key === th.dataset.key ? -sort.dir : (th.classList.contains('num') ? -1 : 1) };
        build();
      };
    });
  };
  build();
  fitPage();
}

/* ── polling ────────────────────────────────────────────────────────────── */
let perfBusy = false, procBusy = false;
async function perfTick() {
  if (perfBusy) return; perfBusy = true;
  try {
    const p = await window.api.perfSample();
    lastPerf = p;
    updateHist(p);
    drawGraphs();
    renderCpu(p);
    renderMem(p);
    renderGpuSection(p);
    renderDiskSection(p);
    renderNetSection(p);
    updateFooters();
    fitPage(); // content heights can change as data lands (stats rows, blocks)
  } catch (e) { console.error('perf', e); } finally { perfBusy = false; }
}
async function procTick() {
  if (procBusy) return; procBusy = true;
  try {
    const pr = await window.api.procSample();
    if (pr.selfUid != null) selfUid = pr.selfUid;
    lastProcs = pr;
    tasksTable.render(pr.procs);
    updateFooters();
  } catch (e) { console.error('proc', e); } finally { procBusy = false; }
}
function updateFooters() {
  const n = lastProcs?.procs.length ?? 0;
  document.getElementById('tasks-count').textContent = n;
  const cpu = lastPerf?.cpu.usage, mem = lastPerf?.memory;
  const txt = `${n} processes` + (cpu != null ? ` · CPU ${Math.round(cpu)}% · Memory ${fBytes(mem.inUse)} of ${fBytes(mem.total)}` : '');
  document.getElementById('tasks-foot').textContent = txt;
}

window.api.meta().then(m => { document.getElementById('nav-host').textContent = m.hostname; });

/* ── graph drawing ──────────────────────────────────────────────────────── */
// No animation loop: canvases are redrawn only when new data arrives
// (every PERF_MS) — plus on hover, resize, and tab switch (ResizeObserver) —
// so GPU compositing work happens at the data rate (~2 fps), which is
// negligible.
function drawGraphs() {
  for (const g of graphs.values()) g.render();
}

// error capture (verification + debugging)
window.__errors = [];
window.addEventListener('error', e => window.__errors.push(String(e.message || e)));
window.addEventListener('unhandledrejection', e => window.__errors.push('promise: ' + String(e.reason)));

setInterval(() => { if (!document.hidden) perfTick(); }, PERF_MS);
setInterval(() => { if (!document.hidden) procTick(); }, PROC_MS);
perfTick();
procTick();
