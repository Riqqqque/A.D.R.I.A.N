// Times the orb in headless Edge (never a visible window) through the DevTools
// protocol: frames drawn per second under a simulated monitor, script time per
// frame, and the CPU time of every Edge process (browser, renderer, GPU) while
// it runs. Or, with --ref, draws two orb.js versions on one fake clock and
// compares their pixels.
//
//   node orb-bench.mjs [--orb ../ui/orb.js] [--ref old.js] [--hz 120] [--secs 20] [--dpr 1.25] [--runs 3] [--out r.json]
//   node orb-bench.mjs --orb ../ui/orb.js --ref old/orb.js --compare
//
// Needs Node 22+ (global WebSocket) and Microsoft Edge.

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => {
    if (a.startsWith('--')) acc.push([a.slice(2), all[i + 1] && !all[i + 1].startsWith('--') ? all[i + 1] : '1']);
    return acc;
  }, []),
);
const orb = resolve(args.orb || join(here, '../ui/orb.js'));
const hz = Number(args.hz || 120);
const secs = Number(args.secs || 20);
const dpr = Number(args.dpr || 1.25);
const runs = Number(args.runs || 1);
const compare = !!args.compare;

const EDGE = [
  process.env.EDGE,
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
].find((p) => p && existsSync(p));
if (!EDGE) throw new Error('Edge not found (set EDGE)');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function session() {
  const profile = mkdtempSync(join(tmpdir(), 'orb-bench-'));
  const edge = spawn(EDGE, [
    '--headless=new', `--user-data-dir=${profile}`, '--remote-debugging-port=0',
    '--no-first-run', '--no-default-browser-check', '--disable-extensions', '--mute-audio',
    '--allow-file-access-from-files', '--window-size=460,600', `--force-device-scale-factor=${dpr}`,
    // keep timers and drawing going in a page nobody looks at
    '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows',
    'about:blank',
  ], { stdio: 'ignore' });
  let port;
  for (let i = 0; i < 100 && !port; i++) {
    await sleep(100);
    try { port = readFileSync(join(profile, 'DevToolsActivePort'), 'utf8').split('\n')[0].trim(); } catch {}
  }
  if (!port) throw new Error('Edge did not open its debugging port');
  const version = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
  const ws = new WebSocket(version.webSocketDebuggerUrl);
  await new Promise((ok, bad) => { ws.onopen = ok; ws.onerror = bad; });
  let id = 0;
  const waiting = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && waiting.has(msg.id)) {
      const { ok, bad } = waiting.get(msg.id);
      waiting.delete(msg.id);
      msg.error ? bad(new Error(JSON.stringify(msg.error))) : ok(msg.result);
    }
  };
  const send = (method, params = {}, sessionId) => new Promise((ok, bad) => {
    const n = ++id;
    waiting.set(n, { ok, bad });
    ws.send(JSON.stringify({ id: n, method, params, ...(sessionId ? { sessionId } : {}) }));
  });
  const close = async () => {
    try { await send('Browser.close'); } catch {}
    ws.close();
    await sleep(500);
    try { edge.kill(); } catch {}
    for (let i = 0; i < 10; i++) {
      try { rmSync(profile, { recursive: true, force: true }); break; } catch { await sleep(300); }
    }
  };
  return { send, close, pid: edge.pid };
}

const cpuByType = (info) => {
  const out = {};
  for (const p of info.processInfo) out[p.type] = (out[p.type] || 0) + p.cpuTime;
  return out;
};

async function once(s, url) {
  const { targetId } = await s.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await s.send('Target.attachToTarget', { targetId, flatten: true });
  await s.send('Performance.enable', {}, sessionId);
  // The app's window: 460 x 600 at the main monitor's 125%.
  await s.send('Emulation.setDeviceMetricsOverride', { width: 460, height: 600, deviceScaleFactor: dpr, mobile: false }, sessionId);
  if (args.profile) { await s.send("Profiler.enable", {}, sessionId); await s.send("Profiler.start", {}, sessionId); }
  await s.send("Page.navigate", { url }, sessionId);
  const evalJs = async (expr) => (await s.send('Runtime.evaluate', { expression: expr, returnByValue: true }, sessionId)).result.value;
  for (let i = 0; i < 100 && !(await evalJs('window.__bench && (window.__bench.started || window.__bench.done)')); i++) await sleep(50);
  const cpu0 = cpuByType(await s.send('SystemInfo.getProcessInfo'));
  const w0 = Date.now();
  let r;
  for (;;) {
    await sleep(250);
    r = await evalJs('window.__bench');
    if (r && r.done) break;
  }
  const wall = (Date.now() - w0) / 1000;
  const cpu1 = cpuByType(await s.send('SystemInfo.getProcessInfo'));
  if (args.profile) {
    // Self time by function and line, from the sampling profiler.
    const { profile } = await s.send("Profiler.stop", {}, sessionId);
    const self = new Map();
    const dts = profile.timeDeltas;
    const byId = new Map(profile.nodes.map((n) => [n.id, n]));
    profile.samples.forEach((id, i) => {
      const f = byId.get(id).callFrame;
      const key = `${f.functionName || "(anon)"} ${f.url.split("/").pop()}:${f.lineNumber + 1}`;
      self.set(key, (self.get(key) || 0) + (dts[i] || 0) / 1000);
    });
    r.profile = [...self].sort((a, b) => b[1] - a[1]).slice(0, 15).map(([k, ms]) => `${ms.toFixed(0).padStart(6)} ms  ${k}`);
  }
  const metrics = Object.fromEntries((await s.send('Performance.getMetrics', {}, sessionId)).metrics.map((m) => [m.name, m.value]));
  await s.send('Target.closeTarget', { targetId });
  if (r.error) throw new Error(r.error);
  if (!compare) {
    r.cpuMsPerSec = Object.fromEntries(Object.keys(cpu1).map((k) => [k, +(((cpu1[k] - (cpu0[k] || 0)) * 1000) / wall).toFixed(1)]));
    r.cpuMsPerSec.total = +Object.values(r.cpuMsPerSec).reduce((a, b) => a + b, 0).toFixed(1);
    r.cpuMsPerDrawn = +((r.cpuMsPerSec.total) / r.fps).toFixed(2);
    r.jsHeapMB = +(metrics.JSHeapUsedSize / 1048576).toFixed(2);
  }
  return r;
}

const s = await session();
try {
  const info = await s.send('SystemInfo.getInfo');
  const gpu = info.gpu.devices.map((d) => d.deviceString).join(', ');
  const raster = info.gpu.featureStatus?.['2d_canvas'];
  const page = pathToFileURL(join(here, 'orb-bench.html')).href;
  const q = new URLSearchParams({ orb: pathToFileURL(orb).href, hz, secs });
  let out;
  if (compare) {
    q.set('compare', '1');
    q.set('ref', pathToFileURL(resolve(args.ref)).href);
    if (args.refhz) q.set('refhz', args.refhz);
    const r = await once(s, `${page}?${q}`);
    const byPhase = {};
    for (const x of r.shots) (byPhase[x.phase] ??= []).push(x);
    const phases = Object.fromEntries(Object.entries(byPhase).map(([k, v]) => [k, {
      meanAbs: +(v.reduce((a, x) => a + x.meanAbs, 0) / v.length).toFixed(3),
      worstMeanAbs: Math.max(...v.map((x) => x.meanAbs)),
      minPsnr: Math.min(...v.map((x) => x.psnr)),
      over8pct: +(v.reduce((a, x) => a + x.over8, 0) / v.length).toFixed(3),
    }]));
    out = { gpu, canvas2d: raster, orb, ref: args.ref, hz, refhz: Number(args.refhz) || hz, canvas: r.canvas, dpr: r.dpr, phases, shots: r.shots };
  } else {
    // With --ref, the two take turns (A, B, A, B...), so a busy moment hits both.
    // --base empty-orb.js: what headless Edge costs with nothing drawn, to take off the others.
    const files = { orb, ...(args.ref ? { ref: resolve(args.ref) } : {}), ...(args.base ? { base: resolve(args.base) } : {}) };
    const all = Object.fromEntries(Object.keys(files).map((k) => [k, []]));
    for (let i = 0; i < runs; i++) {
      for (const [k, f] of Object.entries(files)) {
        q.set('orb', pathToFileURL(f).href);
        all[k].push(await once(s, `${page}?${q}`));
      }
    }
    const summary = (list) => {
      const med = (f) => { const v = list.map(f).sort((a, b) => a - b); return v[Math.floor(v.length / 2)]; };
      return {
        median: {
          fps: +med((r) => r.fps).toFixed(1),
          tickHz: +med((r) => r.tickHz).toFixed(1),
          jsMsPerDrawn: +med((r) => r.jsMsPerDrawn).toFixed(3),
          jsMsPerSec: +med((r) => r.jsMsPerSec).toFixed(1),
          cpuMsPerSec: +med((r) => r.cpuMsPerSec.total).toFixed(1),
          rendererMsPerSec: +med((r) => r.cpuMsPerSec.renderer || 0).toFixed(1),
          gpuProcessCpuMsPerSec: +med((r) => r.cpuMsPerSec.GPU || 0).toFixed(1),
          cpuMsPerDrawn: +med((r) => r.cpuMsPerDrawn).toFixed(2),
          jsHeapMB: med((r) => r.jsHeapMB),
        },
        runs: list.map((r) => ({ fps: +r.fps.toFixed(1), tickHz: +r.tickHz.toFixed(1), jsMsPerDrawn: +r.jsMsPerDrawn.toFixed(3), cpu: r.cpuMsPerSec,
          phases: Object.fromEntries(Object.entries(r.phases).map(([k, v]) => [k, { fps: +v.fps.toFixed(1), jsMsPerDrawn: +v.jsMsPerDrawn.toFixed(3) }])), ...(r.profile ? { profile: r.profile } : {}) })),
      };
    };
    out = { gpu, canvas2d: raster, hz, secs, runs, canvas: all.orb[0].canvas, dpr: all.orb[0].dpr,
      note: 'cpu = CPU time of each Edge process type; GPU is the CPU time of the GPU process, not GPU load',
      ...Object.fromEntries(Object.entries(files).map(([k, f]) => [k, { file: f, ...summary(all[k]) }])) };
    if (out.base) {
      // The orb's own cost: CPU above the empty page, per second and per frame drawn.
      for (const k of ['orb', 'ref']) {
        if (!out[k]) continue;
        const m = out[k].median, b = out.base.median;
        const perSec = m.cpuMsPerSec - b.cpuMsPerSec;
        out[k].overBase = { cpuMsPerSec: +perSec.toFixed(1), cpuMsPerDrawn: +(perSec / m.fps).toFixed(2) };
      }
    }
  }
  const text = JSON.stringify(out, null, 1);
  if (args.out) writeFileSync(args.out, text);
  console.log(text);
} finally {
  await s.close();
}
