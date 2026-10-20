// Treemap renderer for the disk-space grid (Disks tab → click a disk).
// Squarified layout (Bruls, Huizing & van Wijk) on a 2D canvas: each entry
// gets a rectangle whose area is proportional to its size; directories and
// files are tinted differently. Drill-down navigation lives in app.js, which
// feeds one directory's children to setData() per view.
'use strict';
(() => {
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));

  function worstRatio(row, sum, side) {
    const w = sum / side; // thickness of this row
    let worst = 0;
    for (const r of row) {
      const len = r.area / w;
      worst = Math.max(worst, w / len, len / w);
    }
    return worst;
  }

  // entries: [{size, ...}] sorted descending — returns same objects with
  // x/y/w/h layout rectangles attached.
  function squarify(entries, rx, ry, rw, rh) {
    const out = [];
    const total = entries.reduce((s, e) => s + e.size, 0);
    if (total <= 0 || rw <= 0 || rh <= 0) return out;
    const scale = (rw * rh) / total;
    const items = entries.map((e) => ({ e, area: e.size * scale }));
    let x = rx, y = ry, w = rw, h = rh, i = 0;
    while (i < items.length) {
      const side = Math.min(w, h);
      const row = [items[i]];
      let sum = row[0].area;
      let worst = worstRatio(row, sum, side);
      i++;
      while (i < items.length) {
        const nsum = sum + items[i].area;
        const nrow = row.concat(items[i]);
        const nw = worstRatio(nrow, nsum, side);
        if (nw <= worst && items[i].area > 0) {
          row.push(items[i]); sum = nsum; worst = nw; i++;
        } else break;
      }
      if (w >= h) { // vertical strip on the left
        const t = sum / h;
        let yy = y;
        for (const it of row) {
          const hh = it.area / t;
          out.push({ e: it.e, x, y: yy, w: t, h: hh });
          yy += hh;
        }
        x += t; w -= t;
      } else {      // horizontal strip on top
        const t = sum / w;
        let xx = x;
        for (const it of row) {
          const ww = it.area / t;
          out.push({ e: it.e, x: xx, y, w: ww, h: t });
          xx += ww;
        }
        y += t; h -= t;
      }
    }
    return out;
  }

  function fitText(ctx, text, maxW) {
    if (ctx.measureText(text).width <= maxW) return text;
    while (text.length > 1 && ctx.measureText(text + '…').width > maxW) text = text.slice(0, -1);
    return text + '…';
  }

  // Colour ramp: dirs bluish (accent), files amber (disk colour); intensity
  // follows relative rank so the biggest items pop first.
  function ramp(dir, frac) {
    const a = 0.18 + 0.62 * frac;
    return dir ? `rgba(76,194,255,${a.toFixed(3)})` : `rgba(255,200,96,${a.toFixed(3)})`;
  }

  class TreemapGrid {
    constructor(canvas, tipEl, opts = {}) {
      this.cv = canvas;
      this.ctx = canvas.getContext('2d');
      this.tip = tipEl;
      this.onOpen = opts.onOpen || (() => {});
      this.rects = [];
      this.hover = -1;
      this.entries = [];
      this._raf = 0;
      canvas.addEventListener('mousemove', (ev) => this._move(ev));
      canvas.addEventListener('mouseleave', () => this._leave());
      canvas.addEventListener('click', (ev) => this._click(ev));
      const ro = new ResizeObserver(() => this.draw());
      ro.observe(canvas.parentElement);
    }

    // entries: [{name, kind, size, ...}] sorted descending; dirBytes/ownBytes
    // describe the parent directory (for the "... more" bucket).
    setData(entries, opts = {}) {
      this.entries = entries;
      this.hover = -1;
      this.tip.hidden = true;
      this.draw();
    }

    clear() {
      this.entries = [];
      this.rects = [];
      this.hover = -1;
      this.tip.hidden = true;
      const c = this.cv, ctx = this.ctx;
      ctx.clearRect(0, 0, c.width, c.height);
    }

    _css() {
      return getComputedStyle(document.documentElement);
    }

    draw() {
      const c = this.cv;
      const box = c.parentElement;
      const w = Math.max(50, box.clientWidth), h = Math.max(50, box.clientHeight);
      const dpr = window.devicePixelRatio || 1;
      if (c.width !== Math.round(w * dpr) || c.height !== Math.round(h * dpr)) {
        c.width = Math.round(w * dpr);
        c.height = Math.round(h * dpr);
      }
      const ctx = this.ctx;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      this.rects = [];
      if (!this.entries.length) return;

      const PAD = 1; // gap between tiles
      const pos = new Map();
      this.entries.forEach((e, i) => pos.set(e, i));
      const laid = squarify(this.entries, PAD, PAD, w - 2 * PAD, h - 2 * PAD);
      const n = this.entries.length;
      for (const r of laid) {
        // rank fraction from position in the sorted list (log-ish feel)
        const frac = 1 - (pos.get(r.e) / Math.max(1, n - 1));
        const dir = r.e.kind === 'dir';
        ctx.fillStyle = ramp(dir, frac);
        const x = Math.round(r.x), y = Math.round(r.y);
        const rw = Math.max(0, Math.round(r.x + r.w) - x - PAD);
        const rh = Math.max(0, Math.round(r.y + r.h) - y - PAD);
        if (rw <= 0 || rh <= 0) continue;
        ctx.fillRect(x, y, rw, rh);
        if (dir) {
          ctx.strokeStyle = 'rgba(76,194,255,.45)';
          ctx.lineWidth = 1;
          ctx.strokeRect(x + 0.5, y + 0.5, rw - 1, rh - 1);
        }
        const idx = this.rects.length;
        this.rects.push({ ...r, px: x, py: y, pw: rw, ph: rh, idx });
        // labels
        if (rw > 46 && rh > 26) {
          ctx.fillStyle = 'rgba(0,0,0,.55)';
          ctx.font = '600 11px "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif';
          const label = fitText(ctx, r.e.name, rw - 10);
          ctx.fillText(label, x + 5, y + 15);
          if (rh > 42 && r.e.fmtSize) {
            ctx.fillStyle = 'rgba(0,0,0,.5)';
            ctx.font = '10px "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif';
            const sz = fitText(ctx, r.e.fmtSize, rw - 10);
            ctx.fillText(sz, x + 5, y + 29);
          }
        }
        if (r.e.kind === 'other') {
          ctx.fillStyle = 'rgba(255,255,255,.06)';
          ctx.fillRect(x, y, rw, rh);
        }
        if (idx === this.hover) {
          ctx.strokeStyle = '#fff';
          ctx.lineWidth = 2;
          ctx.strokeRect(x + 1, y + 1, rw - 2, rh - 2);
        }
      }
    }

    _pick(ev) {
      const b = this.cv.getBoundingClientRect();
      const x = ev.clientX - b.left, y = ev.clientY - b.top;
      return this.rects.findIndex((r) => x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h);
    }

    _move(ev) {
      const i = this._pick(ev);
      if (i === this.hover) return;
      this.hover = i;
      if (i < 0) { this.tip.hidden = true; this.draw(); return; }
      const e = this.rects[i].e;
      const tip = this.tip;
      tip.innerHTML = `<b>${esc(e.name)}</b>` + (e.fmtSize ? `<br>${esc(e.fmtSize)}` : '') +
        (e.kind === 'dir' ? '<br><span class="muted">Click to open</span>' : '');
      tip.hidden = false;
      const b = this.cv.getBoundingClientRect();
      const tb = tip.getBoundingClientRect();
      let tx = ev.clientX - b.left + 14, ty = ev.clientY - b.top + 14;
      if (tx + tb.width > b.width - 8) tx = ev.clientX - b.left - tb.width - 10;
      if (ty + tb.height > b.height - 8) ty = ev.clientY - b.top - tb.height - 10;
      tip.style.left = `${Math.max(4, tx)}px`;
      tip.style.top = `${Math.max(4, ty)}px`;
      this.draw();
    }

    _leave() {
      this.hover = -1;
      this.tip.hidden = true;
      this.draw();
    }

    _click(ev) {
      const i = this._pick(ev);
      if (i < 0) return;
      const e = this.rects[i].e;
      if (e.kind === 'dir') this.onOpen(e);
    }
  }

  window.TreemapGrid = TreemapGrid;
})();
