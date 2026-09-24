// Tauri bridge: exposes the window.api surface the UI uses, on top of the
// injected __TAURI_INTERNALS__ core invoke.
// Loaded before app.js; no npm dependencies required.
'use strict';
(() => {
  const core = window.__TAURI_INTERNALS__;
  if (!core || typeof core.invoke !== 'function') {
    console.error('Tauri internals not found — bridge unavailable');
    return;
  }
  const invoke = (cmd, args) => core.invoke(cmd, args);
  const win = (cmd) => invoke(`plugin:window|${cmd}`, { label: 'main' });

  window.api = {
    perfSample: () => invoke('perf_sample'),
    procSample: () => invoke('proc_sample'),
    killProcess: (pid, force) => invoke('proc_kill', { pid, force }),
    meta: () => invoke('meta_get'),
    logStart: () => invoke('log_start'),
    logStop: () => invoke('log_stop'),
    logStatus: () => invoke('log_status'),
    win: {
      min: () => win('minimize'),
      maxToggle: async () => {
        const max = await win('is_maximized');
        return win(max ? 'unmaximize' : 'maximize');
      },
      close: () => win('close'),
      drag: () => win('start_dragging'),
      resize: (value) => invoke('plugin:window|start_resize_dragging', { label: 'main', value }),
    },
  };

  /* ── window chrome (WebKitGTK ignores -webkit-app-region: drag, so the
        titlebar drag + edge/corner resize handles are driven by Tauri's
        start_dragging / start_resize_dragging instead) ─────────────────── */
  const titlebar = document.querySelector('.titlebar');
  if (titlebar) {
    titlebar.addEventListener('mousedown', (e) => {
      if (e.button !== 0) return;
      if (e.target.closest('.win-buttons')) return;
      window.api.win.drag();
    });
  }

  const HANDLES = [
    ['n', 'North'], ['e', 'East'], ['s', 'South'], ['w', 'West'],
    ['ne', 'NorthEast'], ['nw', 'NorthWest'], ['se', 'SouthEast'], ['sw', 'SouthWest'],
  ];
  const css = document.createElement('style');
  css.textContent = `
    .win-rs { position: fixed; z-index: 9999; }
    .win-rs-n { top: 0; left: 14px; right: 14px; height: 5px; cursor: n-resize; }
    .win-rs-s { bottom: 0; left: 14px; right: 14px; height: 5px; cursor: s-resize; }
    .win-rs-w { left: 0; top: 14px; bottom: 14px; width: 5px; cursor: w-resize; }
    .win-rs-e { right: 0; top: 14px; bottom: 14px; width: 5px; cursor: e-resize; }
    .win-rs-nw { top: 0; left: 0; width: 14px; height: 14px; cursor: nwse-resize; }
    .win-rs-ne { top: 0; right: 0; width: 14px; height: 14px; cursor: nesw-resize; }
    .win-rs-sw { bottom: 0; left: 0; width: 14px; height: 14px; cursor: nwse-resize; }
    .win-rs-se { bottom: 0; right: 0; width: 14px; height: 14px; cursor: nesw-resize; }
  `;
  document.head.appendChild(css);
  for (const [edge, value] of HANDLES) {
    const h = document.createElement('div');
    h.className = `win-rs win-rs-${edge}`;
    h.setAttribute('aria-hidden', 'true');
    h.addEventListener('mousedown', (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      window.api.win.resize(value);
    });
    document.body.appendChild(h);
  }
})();
