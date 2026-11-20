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
// It draws at most about 60 times a second whatever the monitor's refresh: on a
// 120 Hz (or faster) screen it draws every second (or nth) refresh, and the
// motion is timed by the clock, so it looks the same at any rate.

(function (global) {
  'use strict';

  const RINGS = 23;
  const MODES = ['idle', 'listening', 'thinking', 'speaking'];

  const rgb = (hex) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
  const col = (c, a) => `rgba(${c[0]},${c[1]},${c[2]},${Math.max(0, Math.min(1, a))})`;
  const mix = (a, b, k) => a.map((v, i) => Math.round(v + (b[i] - v) * k));
  const WHITE = [255, 255, 255];
  /** The motion was tuned frame by frame on a 120 Hz screen; easing is by time now, scaled to that. */
  const TUNED_HZ = 120;
  /** Draw about this often, at most. */
  const MAX_FPS = 60;
  /** Per-frame easing `k` at TUNED_HZ, as the share to move after `dt` seconds. */
  const ease = (k, dt) => 1 - Math.pow(1 - Math.min(1, k), dt * TUNED_HZ);

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
      const key = ((c[0] << 16) | (c[1] << 8) | c[2]) + Math.round(hot * 100) * 16777216;
      let s = sprites.get(key);
      if (!s) sprites.set(key, (s = makeSprite(c, hot)));
      return s;
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
    // The refresh rate, learned from the callbacks, and how many to let pass between drawings.
    let prevTick = 0;
    let tickAvg = 0;
    let skipped = 0;
    let every = 1;
    const tgt = new Float32Array(RINGS);

    // Dust: far specks, nearer specks, and a few large out-of-focus motes.
    const DUST = Array.from({ length: 230 }, (_, i) => {
      const z = i < 150 ? 0.15 + 0.35 * hash(i, 3) : i < 222 ? 0.5 + 0.4 * hash(i, 3) : 1;
      const ph = hash(i, 4) * 6.28;
      return { x: hash(i, 1), y: hash(i, 2), z, ph, big: i >= 222, size: 40 + 50 * hash(ph * 10, 5) };
    });

    // Each ring's fixed shape: its height, radius, and the angles of its points.
    const turn = (n) => {
      const c = new Float64Array(n + 1), s = new Float64Array(n + 1);
      for (let k = 0; k <= n; k++) {
        const th = (k / n) * Math.PI * 2;
        c[k] = Math.cos(th);
        s[k] = Math.sin(th);
      }
      return { c, s };
    };
    const OUTLINE = turn(64);
    const RING = Array.from({ length: RINGS }, (_, i) => {
      const v = (i + 0.5) / RINGS;
      const y0 = 1 - 2 * v;
      const rr = Math.sqrt(1 - y0 * y0);
      const n = Math.max(14, Math.round(86 * rr));
      return { v, e: Math.abs(v - 0.5) * 2, y0, rr, n, dots: turn(n) };
    });
    let vignette = null;

    function fit() {
      dpr = Math.min(2, global.devicePixelRatio || 1);
      const w = Math.round(canvas.clientWidth * dpr), h = Math.round(canvas.clientHeight * dpr);
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
        layer.width = w;
        layer.height = h;
        glowCanvas.width = Math.max(1, Math.round(w / 2));
        glowCanvas.height = Math.max(1, Math.round(h / 2));
        vignette = null;
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
      g.addColorStop(0, col(accent, 0.11 + 0.06 * s.speak + 0.03 * s.listen));
      g.addColorStop(0.45, col(accent, 0.026));
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
        const dx = px - cx, dy = py - cy, dist = Math.sqrt(dx * dx + dy * dy) || 1;
        const push = 18 * dpr * s.slowSpeak * d.z;
        const fx = px + (dx / dist) * push, fy = py + (dy / dist) * push;
        const tw = 0.55 + 0.45 * Math.sin(t * (0.6 + d.z * 1.4) + d.ph);
        if (d.big) {
          const sz = d.size * dpr;
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
      if (!vignette) {
        vignette = ctx.createRadialGradient(w / 2, h / 2, m * 0.28, w / 2, h / 2, m * 0.82);
        vignette.addColorStop(0, 'rgba(0,0,0,0)');
        vignette.addColorStop(1, 'rgba(0,0,0,0.5)');
      }
      ctx.fillStyle = vignette;
      ctx.fillRect(0, 0, w, h);
    }

    // ---- the bands ------------------------------------------------------
    /** What each ring should do right now: 0 = resting width. */
    function targets(t, s, out) {
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
      const tick = dt * TUNED_HZ;
      rot += (0.0022 + 0.004 * W.thinking + 0.0015 * W.speaking) * step * (reduce ? 0.3 : 1);
      targets(t, s, tgt);
      const scan = (t * 0.85) % 1.25 - 0.12;
      lctx.setTransform(1, 0, 0, 1, 0, 0);
      lctx.clearRect(0, 0, w, h);
      lctx.globalCompositeOperation = 'lighter';
      // A fixed three-quarter view from slightly above.
      const tilt = 0.42;
      const ct = Math.cos(tilt), st = Math.sin(tilt);
      const b = 0.85 + 0.25 * W.listening * (0.6 + level) + 0.45 * W.speaking * (0.45 + level) + 0.15 * W.idle + 0.12 * W.thinking;

      for (let i = 0; i < RINGS; i++) {
        // EQ feel: fast attack, slow release; peak caps hang and fall.
        const k = tgt[i] > width[i] ? 0.42 : 0.075;
        width[i] += (tgt[i] - width[i]) * (1 - Math.pow(1 - Math.min(1, k * 0.5), tick));
        peak[i] = Math.max(width[i], peak[i] - 0.0035 * step);
        // while thinking, alternate rings counter-rotate
        ringTurn[i] += (i % 2 ? 1 : -1) * 0.018 * W.thinking * step * (reduce ? 0.3 : 1);

        const { v, e, y0, rr, n, dots } = RING[i];
        const wide = 1 + width[i];
        const lit = W.thinking * Math.exp(-Math.pow((v - scan) / 0.045, 2));
        const ringCol = mix(accent, WHITE, 0.4 * (1 - e) + 0.4 * lit);
        const spr = sprite(ringCol, 0.5);
        const a0 = rot + ringTurn[i] + i * 0.31;
        const ca = Math.cos(a0), sa = Math.sin(a0);
        const yc = y0 * ct, ys = y0 * st;

        // a hairline for the ring itself, and a peak cap above it while she talks
        const outline = 0.07 + 0.06 * W.speaking + 0.25 * lit;
        const cap = Math.min(0.35, (peak[i] - width[i]) * 3) * (W.speaking + W.listening * 0.6);
        for (let pass = 0; pass < 2; pass++) {
          const scale = pass ? 1 + peak[i] + 0.012 : wide;
          const alpha = pass ? cap : outline;
          if (alpha < 0.01) continue;
          lctx.beginPath();
          for (let k2 = 0; k2 <= 64; k2++) {
            const x = OUTLINE.c[k2] * rr * scale, z = OUTLINE.s[k2] * rr * scale;
            const x1 = x * ca + z * sa, z1 = -x * sa + z * ca;
            const y1 = yc - z1 * st, z2 = ys + z1 * ct;
            const f = 1 / (1 - 0.2 * z2);
            const px = cx + x1 * R * f, py = cy + y1 * R * f;
            k2 ? lctx.lineTo(px, py) : lctx.moveTo(px, py);
          }
          lctx.strokeStyle = col(ringCol, alpha * b);
          lctx.lineWidth = (pass ? 1.1 : 0.9) * dpr;
          lctx.stroke();
        }

        // the ring's points, one sprite each
        const grow = Math.max(0.8, Math.min(2, R / (70 * dpr)));
        const sizeK = dpr * 1.9 * grow;
        const dim = 1 - 0.1 * e;
        for (let k2 = 0; k2 < n; k2++) {
          const x = dots.c[k2] * rr * wide, z = dots.s[k2] * rr * wide;
          const x1 = x * ca + z * sa, z1 = -x * sa + z * ca;
          const y1 = yc - z1 * st, z2 = ys + z1 * ct;
          const f = 1 / (1 - 0.2 * z2);
          const depth = (z2 + 1) / 2;
          const alpha = ((0.14 + 0.78 * depth * Math.sqrt(depth)) * b + lit * 0.7) * dim;
          if (alpha < 0.005) continue;
          const sz = (0.85 + 1.25 * depth + 1.3 * lit + 0.8 * width[i]) * f * sizeK;
          lctx.globalAlpha = Math.min(1, alpha);
          lctx.drawImage(spr, cx + x1 * R * f - sz / 2, cy + y1 * R * f - sz / 2, sz, sz);
        }
        lctx.globalAlpha = 1;
      }

      // bloom: a half-size blurred copy laid over the sharp one
      glowCtx.setTransform(1, 0, 0, 1, 0, 0);
      glowCtx.clearRect(0, 0, glowCanvas.width, glowCanvas.height);
      glowCtx.filter = `blur(${Math.max(4, 6 * dpr)}px)`;
      glowCtx.drawImage(layer, 0, 0, glowCanvas.width, glowCanvas.height);
      glowCtx.filter = 'none';
      ctx.globalCompositeOperation = 'lighter';
      ctx.globalAlpha = 0.95 + 0.4 * s.speak;
      ctx.drawImage(glowCanvas, 0, 0, w, h);
      ctx.globalAlpha = 1;
      ctx.drawImage(layer, 0, 0);
      ctx.globalCompositeOperation = 'source-over';
    }

    function frame(ms) {
      raf = global.requestAnimationFrame(frame);
      const now = ms / 1000;
      // Learn the refresh rate (ignoring pauses) and let the extra refreshes pass.
      // Only gaps after a quick callback count: one after a drawing that ran
      // past a refresh would make the screen look slower than it is.
      const gap = prevTick ? now - prevTick : 0;
      prevTick = now;
      if (gap > 0 && gap < 0.1 && (skipped > 0 || every === 1)) tickAvg = tickAvg ? tickAvg + (gap - tickAvg) * 0.05 : gap;
      // Refreshes per drawing, about refresh/60; it moves only when clearly past a
      // boundary, so a 165 Hz screen (right on one) doesn't flip between 55 and 82 fps.
      if (tickAvg) {
        const x = 1 / MAX_FPS / tickAvg + 0.25;
        if (x >= every + 1.1 || x < every - 0.1) every = Math.max(1, Math.floor(x));
      }
      if (++skipped < every) return;
      skipped = 0;
      const dt = last ? Math.max(0, Math.min(0.05, now - last)) : 1 / 60;
      last = now;
      if (!fit()) return;
      const t = reduce ? now * 0.35 : now;
      const k = 1 - Math.pow(0.002, dt);
      for (const m of MODES) W[m] += ((m === mode ? 1 : 0) - W[m]) * k;
      const target = mode === 'speaking' || mode === 'listening' ? inLevel : 0;
      level += (target - level) * ease(target > level ? 0.3 : 0.1, dt);
      slow += (level * W.speaking - slow) * ease(0.04, dt);
      lean.x += (lean.tx - lean.x) * ease(0.06, dt);
      lean.y += (lean.ty - lean.y) * ease(0.06, dt);
      const s = { speak: level * W.speaking, listen: level * W.listening, slowSpeak: slow };
      const w = canvas.width, h = canvas.height;
      const cx = w / 2, cy = h * (opts.centerY ?? 0.46);
      const R = Math.min(w, h) * (opts.scale ?? 0.27);
      backdrop(t, w, h, cx, cy, s);
      drawBands(t, dt, w, h, cx, cy, R, s);
    }

    // The host can pause drawing too (hidden to the tray, a full-screen game in front),
    // since WebView2 doesn't always report those as a hidden page.
    let active = true;
    function onVisibility() {
      if (document.hidden || !active) {
        global.cancelAnimationFrame(raf);
        raf = 0;
      } else if (!raf) {
        last = 0;
        prevTick = 0;
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
      setActive(on) { active = !!on; onVisibility(); },
      destroy() {
        global.cancelAnimationFrame(raf);
        document.removeEventListener('visibilitychange', onVisibility);
      },
    };
  }

  global.createOrb = createOrb;
})(typeof window !== 'undefined' ? window : globalThis);
