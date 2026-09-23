// Canvas line graph: multi-series, area fill, grid, auto-scale, hover tooltip.
//
// The graph is redrawn only when new data arrives (or on hover/resize).
// Each point carries a timestamp {t, v} (t = performance.now(), oldest
// first) and the x-axis is a time window, so the strip steps left once per
// update — no animation loop, no sustained GPU compositing cost. The newest
// value is carried to the right edge as a live edge.
'use strict';

function niceStep(raw) {
  if (raw <= 0) return 1;
  const p = 10 ** Math.floor(Math.log10(raw));
  const n = raw / p;
  const s = n <= 1 ? 1 : n <= 2 ? 2 : n <= 5 ? 5 : 10;
  return s * p;
}

class LineGraph {
  /**
   * @param {HTMLCanvasElement} canvas
   * @param {object} opts
   *   series: [{ color, fill? (0-1 alpha), name?, data: {t,v}[] }]  (shared arrays, newest at end)
   *   mode:  'percent' | 'auto'
   *   fmt:   (v) => string   value formatter
   *   windowMs: visible time window (default 90 s)
   */
  constructor(canvas, opts = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d');
    this.series = opts.series || [];
    this.mode = opts.mode || 'percent';
    this.fmt = opts.fmt || (v => v.toFixed(1));
    this.showGrid = opts.grid !== false;
    this.windowMs = opts.windowMs || 90_000;
    this._mouseX = null;
    this._scale = 100; // y ceiling
    if (opts.hover !== false) {
      canvas.addEventListener('mousemove', e => { this._mouseX = e.offsetX; this.render(); });
      canvas.addEventListener('mouseleave', () => { this._mouseX = null; this.render(); });
    }
    this._tipEl = null;
    this._ro = new ResizeObserver(() => this.render());
    this._ro.observe(canvas);
  }

  _ensureTip() {
    if (this._tipEl) return this._tipEl;
    const tip = document.createElement('div');
    tip.className = 'graph-tip';
    this.canvas.parentElement.appendChild(tip);
    this._tipEl = tip;
    return tip;
  }

  render(now = performance.now()) {
    if (!this.series.some(s => s.data.length)) return; // nothing to draw yet
    const { canvas, ctx } = this;
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth, h = canvas.clientHeight;
    if (!w || !h) return;
    if (canvas.width !== Math.round(w * dpr)) { canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr); }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);

    const winMs = this.windowMs;
    const t0 = now - winMs;
    const padL = 34, padB = 0, padT = 4, padR = 6;
    const gw = w - padL - padR, gh = h - padT - padB;
    const xOf = t => padL + gw * (1 - (now - t) / winMs);

    // visible points per series (arrays are pre-pruned; skip the old tail)
    for (const s of this.series) {
      const d = s.data;
      s._i0 = 0;
      while (s._i0 < d.length - 1 && d[s._i0].t < t0) s._i0++;
    }

    // y scale: grow instantly; shrink only once the old ceiling is clearly
    // excessive (hysteresis — stable at the low redraw rate)
    if (this.mode === 'auto') {
      let mx = 0;
      for (const s of this.series)
        for (let i = s._i0; i < s.data.length; i++) mx = Math.max(mx, s.data[i].v);
      const target = mx <= 0 ? 10 : niceStep(mx * 1.15);
      if (target > this._scale || this._scale > target * 2.2) this._scale = target;
    } else {
      this._scale = 100;
    }
    const scale = this._scale;
    const yOf = v => padT + gh * (1 - Math.min(1, Math.max(0, v / scale)));

    // grid
    if (this.showGrid) {
      ctx.strokeStyle = 'rgba(255,255,255,.055)';
      ctx.lineWidth = 1;
      for (let i = 0; i <= 4; i++) {
        const y = padT + gh * i / 4 + .5;
        ctx.beginPath(); ctx.moveTo(padL, y); ctx.lineTo(w - padR, y); ctx.stroke();
      }
      ctx.fillStyle = 'rgba(255,255,255,.28)';
      ctx.font = '10px "Segoe UI", "Ubuntu", sans-serif';
      ctx.textAlign = 'left';
      for (const frac of [0, .5, 1]) {
        const y = padT + gh * (1 - frac);
        const label = this.mode === 'percent' ? `${Math.round(scale * frac)}%` : this.fmtShort(scale * frac);
        ctx.fillText(label, 4, Math.max(9, Math.min(h - 3, y + 3)));
      }
    }

    // series: area + stroke, with live edge from newest point to the right edge
    const xRight = w - padR;
    for (const s of this.series) {
      const d = s.data;
      const i0 = s._i0, n = d.length;
      if (n === 0) continue;
      // area fill
      if (s.fill && (n - i0) >= 1) {
        ctx.beginPath();
        let started = false;
        for (let i = i0; i < n; i++) {
          const x = xOf(d[i].t), y = yOf(d[i].v);
          started ? ctx.lineTo(x, y) : (ctx.moveTo(x, yOf(0)), ctx.lineTo(x, y), started = true);
        }
        ctx.lineTo(xRight, yOf(d[n - 1].v));   // live edge
        ctx.lineTo(xRight, yOf(0));
        ctx.closePath();
        const grad = ctx.createLinearGradient(0, padT, 0, padT + gh);
        grad.addColorStop(0, hexA(s.color, s.fill));
        grad.addColorStop(1, hexA(s.color, 0.02));
        ctx.fillStyle = grad;
        ctx.fill();
      }
      // stroke
      if (n - i0 >= 1) {
        ctx.beginPath();
        let started = false;
        for (let i = i0; i < n; i++) {
          const x = xOf(d[i].t), y = yOf(d[i].v);
          started ? ctx.lineTo(x, y) : (ctx.moveTo(x, y), started = true);
        }
        ctx.lineTo(xRight, yOf(d[n - 1].v));   // live edge
        ctx.strokeStyle = s.color;
        ctx.lineWidth = 1.4;
        ctx.lineJoin = 'round';
        ctx.stroke();
      }
    }

    // hover crosshair + tooltip
    const tip = this._ensureTip();
    const ref = this.series.find(s => s.data.length > 1);
    if (this._mouseX != null && this._mouseX > padL && this._mouseX < w - padR && ref) {
      // inverse of xOf: left edge = oldest (now - winMs), right edge = now
      const tMouse = now - (1 - (this._mouseX - padL) / gw) * winMs;
      const d = ref.data;
      let best = -1, bd = Infinity;
      for (let i = ref._i0; i < d.length; i++) {
        const dt = Math.abs(d[i].t - tMouse);
        if (dt < bd) { bd = dt; best = i; }
      }
      if (best >= 0) {
        const px = xOf(d[best].t);
        ctx.strokeStyle = 'rgba(255,255,255,.25)';
        ctx.setLineDash([3, 3]);
        ctx.beginPath(); ctx.moveTo(px, padT); ctx.lineTo(px, padT + gh); ctx.stroke();
        ctx.setLineDash([]);
        let rows = '';
        for (const s of this.series) {
          const dd = s.data;
          if (!dd.length) continue;
          const j = Math.max(0, Math.min(dd.length - 1, best + (s._i0 - ref._i0)));
          rows += `<div class="t-line"><i style="background:${s.color}"></i>${s.name ? s.name + ' ' : ''}<span class="t-val">${this.fmt(dd[j].v)}</span></div>`;
        }
        const ageSec = (now - d[best].t) / 1000;
        tip.innerHTML = rows + `<div style="color:var(--text-3);margin-top:2px">${ageSec < 0.75 ? 'now' : '−' + Math.round(ageSec) + ' s'}</div>`;
        tip.style.display = 'block';
        const tw = tip.offsetWidth;
        tip.style.left = Math.min(w - tw - 8, Math.max(4, px + 10)) + 'px';
        tip.style.top = '8px';
      }
    }
    if (!(this._mouseX != null && this._mouseX > padL && this._mouseX < w - padR && ref)) tip.style.display = 'none';
  }

  fmtShort(v) {
    if (v >= 1e9) return (v / 1e9).toFixed(1) + 'G';
    if (v >= 1e6) return (v / 1e6).toFixed(0) + 'M';
    if (v >= 1e3) return (v / 1e3).toFixed(0) + 'K';
    return String(Math.round(v));
  }
}

function hexA(hex, a) {
  const m = hex.replace('#', '');
  const r = parseInt(m.slice(0, 2), 16), g = parseInt(m.slice(2, 4), 16), b = parseInt(m.slice(4, 6), 16);
  return `rgba(${r},${g},${b},${a})`;
}
