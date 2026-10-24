// A.D.R.I.A.N. satellite orb: "Bands" lattice on a "Dust" backdrop.
//
// const orb = createOrb(canvas, { accent: '#9fd4ff' });
// orb.setMode('idle' | 'listening' | 'thinking' | 'speaking');
// orb.setLevel(0..1);          // mic level while listening, her voice while speaking
// orb.setAccent('#ffb46e');
// orb.lean(x, y);              // -0.5..0.5, e.g. from the mouse
// orb.destroy();
//
// Drawing stops while the page is hidden, so a minimized window costs nothing.

(function (global) {
  'use strict';

  const RINGS = 19;
  const MODES = ['idle', 'listening', 'thinking', 'speaking'];

  const rgb = (hex) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
  const col = (c, a) => `rgba(${c[0]},${c[1]},${c[2]},${Math.max(0, Math.min(1, a))})`;
  const mix = (a, b, k) => a.map((v, i) => Math.round(v + (b[i] - v) * k));
  const WHITE = [255, 255, 255];

  /** Deterministic 0..1 noise per index, so the dust looks the same every launch. */
  const hash = (i, n) => {
    const x = Math.sin(i * 91.7 + n * 47.3) * 43758.5453;
    return x - Math.floor(x);
  };

  function makeSprite(c, hot = 0.35) {
    const s = document.createElement('canvas');
    s.width = s.height = 64;
    const g = s.getContext('2d');
    const grad = g.createRadialGradient(32, 32, 0, 32, 32, 32);
    grad.addColorStop(0, 'rgba(255,255,255,1)');
    grad.addColorStop(0.16, col(mix(c, WHITE, hot), 0.95));
    grad.addColorStop(0.42, col(c, 0.32));
    grad.addColorStop(1, col(c, 0));
    g.fillStyle = grad;
    g.fillRect(0, 0, 64, 64);
    return s;
  }

  function createOrb(canvas, opts = {}) {
    const ctx = canvas.getContext('2d');
    const glowCanvas = document.createElement('canvas');
    const glowCtx = glowCanvas.getContext('2d');
    const layer = document.createElement('canvas');
    const lctx = layer.getContext('2d');
    const reduce = global.matchMedia ? global.matchMedia('(prefers-reduced-motion: reduce)').matches : false;

    let accent = rgb(opts.accent || '#9fd4ff');
    let sprites = new Map();
    const sprite = (c, hot) => {
      const key = `${c.join(',')}/${hot}`;
      if (!sprites.has(key)) sprites.set(key, makeSprite(c, hot));
      return sprites.get(key);
    };

    let mode = 'idle';
    let inLevel = 0;
    const W = { idle: 1, listening: 0, thinking: 0, speaking: 0 };
    let level = 0;
    let slow = 0;
    let rot = 0;
    const lean = { x: 0, y: 0, tx: 0, ty: 0 };
    const width = new Float32Array(RINGS);
    const peak = new Float32Array(RINGS);
    const ringTurn = new Float32Array(RINGS);
    let raf = 0;
    let last = 0;
    let dpr = 1;

    // Dust: far specks, nearer specks, and a few large out-of-focus motes.
    const DUST = Array.from({ length: 230 }, (_, i) => {
      const z = i < 150 ? 0.15 + 0.35 * hash(i, 3) : i < 222 ? 0.5 + 0.4 * hash(i, 3) : 1;
      return { x: hash(i, 1), y: hash(i, 2), z, ph: hash(i, 4) * 6.28, big: i >= 222 };
    });

    function fit() {
      dpr = Math.min(2, global.devicePixelRatio || 1);
      const w = Math.round(canvas.clientWidth * dpr), h = Math.round(canvas.clientHeight * dpr);
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
        layer.width = w;
        layer.height = h;
        glowCanvas.width = Math.max(1, Math.round(w / 4));
        glowCanvas.height = Math.max(1, Math.round(h / 4));
      }
      return w > 0 && h > 0;
    }

    // ---- backdrop -------------------------------------------------------
    function backdrop(t, w, h, cx, cy, s) {
      ctx.globalCompositeOperation = 'source-over';
      ctx.globalAlpha = 1;
      ctx.fillStyle = '#06080b';
      ctx.fillRect(0, 0, w, h);
      const m = Math.max(w, h);
      // a cold wash of the accent behind the orb, a little brighter when she talks
      const g = ctx.createRadialGradient(cx, cy, 0, cx, cy, m * 0.8);
      g.addColorStop(0, col(accent, 0.075 + 0.05 * s.speak + 0.02 * s.listen));
      g.addColorStop(0.45, col(accent, 0.018));
      g.addColorStop(1, 'rgba(0,0,0,0)');
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, w, h);

      const speck = sprite(mix(accent, WHITE, 0.65), 0.6);
      const mote = sprite(mix(accent, WHITE, 0.3), 0.1);
      ctx.globalCompositeOperation = 'lighter';
      for (const d of DUST) {
        // slow upward drift, parallax with the lean, pushed outward a touch by her voice
        let x = d.x + lean.x * 0.05 * d.z;
        let y = (d.y - t * 0.004 * (0.3 + d.z)) % 1;
        if (y < 0) y += 1;
        y += lean.y * 0.05 * d.z;
        const px = x * w, py = y * h;
        const dx = px - cx, dy = py - cy, dist = Math.hypot(dx, dy) || 1;
        const push = 18 * dpr * s.slowSpeak * d.z;
        const fx = px + (dx / dist) * push, fy = py + (dy / dist) * push;
        const tw = 0.55 + 0.45 * Math.sin(t * (0.6 + d.z * 1.4) + d.ph);
        if (d.big) {
          const sz = (40 + 50 * hash(d.ph * 10, 5)) * dpr;
          ctx.globalAlpha = 0.035 * tw;
          ctx.drawImage(mote, fx - sz / 2, fy - sz / 2, sz, sz);
        } else {
          const sz = (1.1 + 2.6 * d.z) * dpr;
          ctx.globalAlpha = (0.08 + 0.42 * d.z) * tw;
          ctx.drawImage(speck, fx - sz / 2, fy - sz / 2, sz, sz);
        }
      }
      ctx.globalAlpha = 1;
      ctx.globalCompositeOperation = 'source-over';
      const v = ctx.createRadialGradient(w / 2, h / 2, m * 0.28, w / 2, h / 2, m * 0.82);
      v.addColorStop(0, 'rgba(0,0,0,0)');
      v.addColorStop(1, 'rgba(0,0,0,0.62)');
      ctx.fillStyle = v;
      ctx.fillRect(0, 0, w, h);
    }

    // ---- the bands ------------------------------------------------------
    /** What each ring should do right now: 0 = resting width. */
    function targets(t, s) {
      const out = new Float32Array(RINGS);
      // a formant that slides with the syllables, so speech isn't just "all rings up"
      const f1 = 0.25 + 0.35 * (0.5 + 0.5 * Math.sin(t * 2.7 + Math.sin(t * 1.1) * 1.5));
      const rise = 1 - ((t * 0.7) % 1);
      for (let i = 0; i < RINGS; i++) {
        const v = (i + 0.5) / RINGS;
        const e = Math.abs(v - 0.5) * 2; // 0 at the equator, 1 at the poles
        const idle = 0.022 * (0.5 + 0.5 * Math.sin(v * 6.283 - t * 1.1));
        const flicker = 0.62 + 0.38 * Math.sin(t * 9.3 + i * 1.31 + Math.sin(t * 2.3 + i) * 2);
        const speech = s.speak * ((1 - 0.6 * e) * flicker * 0.32 + 0.22 * Math.exp(-Math.pow((e - f1) / 0.14, 2)));
        const hear = s.listen * (0.1 * (1 - 0.5 * e) * flicker + 0.12 * Math.exp(-Math.pow((v - rise) / 0.1, 2)));
        out[i] = idle * (1 - 0.7 * W.speaking) + speech + hear;
      }
      return out;
    }

    function drawBands(t, dt, w, h, cx, cy, R, s) {
      const step = dt * 60;
      rot += (0.0022 + 0.004 * W.thinking + 0.0015 * W.speaking) * step * (reduce ? 0.3 : 1);
      const tgt = targets(t, s);
      const scan = (t * 0.85) % 1.25 - 0.12;
      lctx.setTransform(1, 0, 0, 1, 0, 0);
      lctx.clearRect(0, 0, w, h);
      lctx.globalCompositeOperation = 'lighter';
      const tilt = 0.3 + lean.y * 0.5;
      const ct = Math.cos(tilt), st = Math.sin(tilt);
      const b = 0.5 + 0.22 * W.listening * (0.6 + level) + 0.5 * W.speaking * (0.45 + level) + 0.14 * W.idle + 0.1 * W.thinking;

      for (let i = 0; i < RINGS; i++) {
        // EQ feel: fast attack, slow release; peak caps hang and fall.
        const k = tgt[i] > width[i] ? 0.42 : 0.075;
        width[i] += (tgt[i] - width[i]) * Math.min(1, k * step);
        peak[i] = Math.max(width[i], peak[i] - 0.0035 * step);
        // while thinking, alternate rings counter-rotate
        ringTurn[i] += (i % 2 ? 1 : -1) * 0.018 * W.thinking * step * (reduce ? 0.3 : 1);

        const v = (i + 0.5) / RINGS;
        const e = Math.abs(v - 0.5) * 2;
        const y0 = 1 - 2 * v;
        const rr = Math.sqrt(1 - y0 * y0);
        const wide = 1 + width[i];
        const lit = W.thinking * Math.exp(-Math.pow((v - scan) / 0.045, 2));
        const ringCol = mix(accent, WHITE, 0.28 * (1 - e) + 0.4 * lit);
        const spr = sprite(ringCol, 0.35);
        const a0 = rot + ringTurn[i] + i * 0.31 + lean.x * 0.6;
        const ca = Math.cos(a0), sa = Math.sin(a0);

        const project = (th, scale) => {
          const x = Math.cos(th) * rr * scale, z = Math.sin(th) * rr * scale;
          const x1 = x * ca + z * sa, z1 = -x * sa + z * ca;
          const y1 = y0 * ct - z1 * st, z2 = y0 * st + z1 * ct;
          const f = 1 / (1 - 0.2 * z2);
          return [cx + x1 * R * f, cy + y1 * R * f, z2, f];
        };

        // a hairline for the ring itself, and a peak cap above it while she talks
        const outline = 0.035 + 0.05 * W.speaking + 0.25 * lit;
        const cap = Math.min(0.35, (peak[i] - width[i]) * 3) * (W.speaking + W.listening * 0.6);
        for (const [scale, alpha, lw] of [[wide, outline, 0.9], [1 + peak[i] + 0.012, cap, 1.1]]) {
          if (alpha < 0.01) continue;
          lctx.beginPath();
          for (let k2 = 0; k2 <= 64; k2++) {
            const p = project((k2 / 64) * Math.PI * 2, scale);
            k2 ? lctx.lineTo(p[0], p[1]) : lctx.moveTo(p[0], p[1]);
          }
          lctx.strokeStyle = col(ringCol, alpha * b);
          lctx.lineWidth = lw * dpr;
          lctx.stroke();
        }

        const n = Math.max(12, Math.round(66 * rr));
        const grow = Math.max(0.8, Math.min(2, R / (70 * dpr)));
        for (let k2 = 0; k2 < n; k2++) {
          const [x, y, z, f] = project((k2 / n) * Math.PI * 2, wide);
          const depth = (z + 1) / 2;
          const alpha = ((0.045 + 0.6 * depth * depth) * b + lit * 0.7) * (1 - 0.15 * e);
          if (alpha < 0.005) continue;
          const sz = (0.85 + 1.25 * depth + 1.3 * lit + 0.8 * width[i]) * f * dpr * 2.2 * grow;
          lctx.globalAlpha = Math.min(1, alpha);
          lctx.drawImage(spr, x - sz / 2, y - sz / 2, sz, sz);
        }
        lctx.globalAlpha = 1;
      }

      // bloom: a quarter-size blurred copy laid over the sharp one
      glowCtx.setTransform(1, 0, 0, 1, 0, 0);
      glowCtx.clearRect(0, 0, glowCanvas.width, glowCanvas.height);
      glowCtx.filter = `blur(${Math.max(1.5, 2.5 * dpr)}px)`;
      glowCtx.drawImage(layer, 0, 0, glowCanvas.width, glowCanvas.height);
      glowCtx.filter = 'none';
      ctx.globalCompositeOperation = 'lighter';
      ctx.globalAlpha = 0.55 + 0.35 * s.speak;
      ctx.drawImage(glowCanvas, 0, 0, w, h);
      ctx.globalAlpha = 1;
      ctx.drawImage(layer, 0, 0);
      ctx.globalCompositeOperation = 'source-over';
    }

    function frame(ms) {
      raf = global.requestAnimationFrame(frame);
      const now = ms / 1000;
      const dt = last ? Math.max(0, Math.min(0.05, now - last)) : 1 / 60;
      last = now;
      if (!fit()) return;
      const t = reduce ? now * 0.35 : now;
      const k = 1 - Math.pow(0.002, dt);
      for (const m of MODES) W[m] += ((m === mode ? 1 : 0) - W[m]) * k;
      const target = mode === 'speaking' || mode === 'listening' ? inLevel : 0;
      level += (target - level) * (target > level ? 0.3 : 0.1);
      slow += (level * W.speaking - slow) * 0.04;
      lean.x += (lean.tx - lean.x) * 0.06;
      lean.y += (lean.ty - lean.y) * 0.06;
      const s = { speak: level * W.speaking, listen: level * W.listening, slowSpeak: slow };
      const w = canvas.width, h = canvas.height;
      const cx = w / 2, cy = h * (opts.centerY ?? 0.46);
      const R = Math.min(w, h) * (opts.scale ?? 0.27);
      backdrop(t, w, h, cx, cy, s);
      drawBands(t, dt, w, h, cx, cy, R, s);
    }

    function onVisibility() {
      if (document.hidden) {
        global.cancelAnimationFrame(raf);
        raf = 0;
      } else if (!raf) {
        last = 0;
        raf = global.requestAnimationFrame(frame);
      }
    }
    document.addEventListener('visibilitychange', onVisibility);
    raf = global.requestAnimationFrame(frame);

    return {
      setMode(m) { if (MODES.includes(m)) mode = m; },
      setLevel(x) { inLevel = Math.max(0, Math.min(1, Number(x) || 0)); },
      setAccent(hex) { accent = rgb(hex); sprites = new Map(); },
      lean(x, y) { lean.tx = x; lean.ty = y; },
      destroy() {
        global.cancelAnimationFrame(raf);
        document.removeEventListener('visibilitychange', onVisibility);
      },
    };
  }

  global.createOrb = createOrb;
})(typeof window !== 'undefined' ? window : globalThis);
