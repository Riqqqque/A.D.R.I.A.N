// The window: the orb driven by her real state, the conversation, typing,
// hold-to-talk, and settings.
(() => {
  'use strict';
  const T = window.__TAURI__;
  const invoke = T.core.invoke;
  const listen = T.event.listen;
  const win = T.window.getCurrentWindow();
  const $ = (id) => document.getElementById(id);

  const orb = createOrb($('orb'), { accent: '#9fd4ff', scale: 0.24, centerY: 0.4 });
  // Paused while the window is in the tray, minimized, or behind a full-screen game.
  let drawHeard = false;
  listen('draw', (e) => { drawHeard = true; orb.setActive(e.payload); });
  invoke('orb_drawing').then((on) => { if (!drawHeard) orb.setActive(on); });

  // --- her state ----------------------------------------------------------
  const LABEL = { idle: 'Ready', listening: 'Listening', thinking: 'Thinking', speaking: 'Speaking' };
  const st = { connected: false, text: 'Starting', mode: 'idle', micError: null };
  function render() {
    const chip = $('chip');
    if (!st.connected) {
      chip.dataset.mode = 'offline';
      $('chipText').textContent = /connect/i.test(st.text) && !/can't|refused|not set/i.test(st.text) ? 'Connecting' : 'Offline';
    } else if (st.micError && st.mode === 'idle') {
      chip.dataset.mode = 'offline';
      $('chipText').textContent = 'No microphone';
    } else {
      chip.dataset.mode = st.mode;
      $('chipText').textContent = LABEL[st.mode];
    }
    chip.title = st.connected ? (st.micError || '') : st.text;
    $('offline').hidden = st.connected || $('chipText').textContent === 'Connecting';
    const busy = st.mode === 'speaking' || st.mode === 'thinking';
    $('btnStop').hidden = !busy;
    $('btnTalk').hidden = busy;
    renderConnection();
  }
  listen('frame', (e) => {
    const f = e.payload;
    orb.setMode(f.mode);
    orb.setLevel(f.level);
    $('micMeter').style.width = `${Math.round(Math.min(1, f.mic) * 100)}%`;
    if (f.mode !== st.mode || f.mic_error !== st.micError) {
      st.mode = f.mode;
      st.micError = f.mic_error;
      render();
    }
  });
  // The mic and speakers she's using: Windows' defaults, followed as they change.
  const showDevices = (d) => {
    if (d && (d.mic || d.speakers)) $('devices').textContent = `Using ${d.mic || 'no mic'} and ${d.speakers || 'no speakers'} (the Windows defaults; she follows them when they change).`;
  };
  listen('devices', (e) => showDevices(e.payload));
  invoke('audio_devices').then(showDevices);
  listen('status', (e) => {
    Object.assign(st, { connected: e.payload.connected, text: e.payload.text });
    render();
  });
  invoke('get_status').then((s) => {
    if (s.text) Object.assign(st, { connected: s.connected, text: s.text });
    render();
  });

  // --- conversation ---------------------------------------------------------
  let fade;
  listen('transcript', (e) => {
    const { who, text, last } = e.payload;
    $('hint').hidden = true;
    if (who === 'you') {
      $('you').textContent = text;
      $('you').hidden = false;
      $('her').hidden = true;
    } else {
      $('her').textContent = text;
      $('her').hidden = false;
      $('her').scrollTop = $('her').scrollHeight;
    }
    clearTimeout(fade);
    if (who === 'her' && last)
      fade = setTimeout(() => {
        $('you').hidden = true;
        $('her').hidden = true;
        $('hint').hidden = false;
      }, 20000);
  });

  // --- typing, talking, stopping -------------------------------------------
  const input = $('askText');
  input.addEventListener('input', () => ($('btnSend').hidden = !input.value.trim()));
  $('ask').addEventListener('submit', (e) => {
    e.preventDefault();
    const t = input.value.trim();
    if (!t) return;
    invoke('send_text', { text: t });
    $('you').textContent = t;
    $('you').hidden = false;
    $('her').hidden = true;
    $('hint').hidden = true;
    input.value = '';
    $('btnSend').hidden = true;
  });
  const talk = $('btnTalk');
  const hold = (on) => {
    talk.classList.toggle('down', on);
    invoke('push_to_talk', { down: on });
  };
  talk.addEventListener('pointerdown', (e) => {
    talk.setPointerCapture(e.pointerId);
    hold(true);
  });
  for (const ev of ['pointerup', 'pointercancel', 'lostpointercapture']) talk.addEventListener(ev, () => talk.classList.contains('down') && hold(false));
  $('btnStop').addEventListener('click', () => invoke('stop_talking'));
  $('offline').addEventListener('click', () => openSettings(true));

  // --- window buttons -------------------------------------------------------
  $('btnMin').addEventListener('click', () => win.minimize());
  // Closing hands the window back entirely; she keeps listening from the tray.
  $('btnHide').addEventListener('click', () => win.close());
  $('btnSettings').addEventListener('click', () => openSettings(false));

  // --- settings ---------------------------------------------------------------
  const PERMS = [
    ['apps', 'Apps and games', 'Open, close and switch between them'],
    ['open', 'Web and folders', 'Websites, YouTube, Downloads and so on'],
    ['media', 'Media', 'Play, pause and skip what is playing'],
    ['volume', 'Volume', 'Overall and per app'],
    ['info', 'PC status', 'CPU, memory and disk space'],
    ['lock', 'Lock the PC', ''],
    ['screen_off', 'Turn the screens off', ''],
    ['sleep', 'Sleep', 'Asks you first'],
    ['restart', 'Restart', 'Asks you first, 30 seconds to cancel'],
    ['shutdown', 'Shut down', 'Asks you first, 30 seconds to cancel'],
  ];
  let current = null;
  const sheet = $('settings');
  const micLabel = (v) => (Number(v) === 0 ? 'Automatic' : `${v} of 60`);
  function renderConnection() {
    if (!current) return;
    const ok = st.connected;
    $('connDot').className = `dot big ${ok ? 'on' : 'off'}`;
    $('connTitle').textContent = ok ? 'Connected to your home server' : current.paired ? 'Not connected' : 'Not set up yet';
    $('connSub').textContent = ok ? 'Her voice, memory and tools are the same as in Discord.' : st.text;
  }
  async function openSettings(showConnection) {
    current = await invoke('get_settings');
    $('server').value = current.server_url;
    $('token').value = '';
    $('token').placeholder = current.paired ? 'Paired. Paste a new token only to replace it.' : 'sat_…';
    $('connDetails').open = showConnection || !current.paired || !current.server_url;
    $('mic').value = Math.round(current.mic_threshold * 600);
    $('micHint').textContent = micLabel($('mic').value);
    $('autoUpdate').checked = current.auto_update;
    const u = current.update;
    $('updateInfo').textContent =
      u.state === 'ready' ? `Version ${u.version}. ${u.latest} is downloaded and installs the next time she's idle in the tray.`
      : u.state === 'available' ? `Version ${u.version}. ${u.latest} is out: turn on automatic updates, or download it from GitHub.`
      : u.state === 'error' ? `Version ${u.version}. The last update check failed: ${u.message}`
      : `Version ${u.version}${u.latest && u.latest === u.version ? ', up to date' : ''}.`;
    const list = $('toggles');
    list.textContent = '';
    for (const [key, label, sub] of PERMS) {
      const row = document.createElement('label');
      row.className = 'switch';
      const text = document.createElement('span');
      text.textContent = label;
      if (sub) {
        const s = document.createElement('span');
        s.className = 'sub';
        s.textContent = sub;
        text.append(s);
      }
      const box = document.createElement('input');
      box.type = 'checkbox';
      box.id = `pc_${key}`;
      box.checked = current.pc[key] !== false;
      row.append(text, box);
      list.append(row);
    }
    $('shortcuts').value = (current.pc.shortcuts || []).map((s) => `${s.name} = ${s.target}${s.args ? ` ${s.args}` : ''}`).join('\n');
    $('error').hidden = true;
    renderConnection();
    sheet.hidden = false;
    $('btnClose').focus();
  }
  function closeSettings() {
    sheet.hidden = true;
  }
  $('mic').addEventListener('input', () => ($('micHint').textContent = micLabel($('mic').value)));
  $('btnClose').addEventListener('click', closeSettings);
  $('btnCancel').addEventListener('click', closeSettings);
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    if (!sheet.hidden) closeSettings();
    else invoke('stop_talking');
  });
  $('btnSave').addEventListener('click', async () => {
    const pc = { ...current.pc };
    for (const [key] of PERMS) pc[key] = $(`pc_${key}`).checked;
    pc.shortcuts = $('shortcuts')
      .value.split('\n')
      .map((l) => l.trim())
      .filter(Boolean)
      .map((l) => {
        const at = l.indexOf('=');
        return at > 0 ? { name: l.slice(0, at).trim(), target: l.slice(at + 1).trim(), args: '' } : null;
      })
      .filter((s) => s && s.name && s.target);
    const save = $('btnSave');
    save.disabled = true;
    save.textContent = 'Saving';
    try {
      await invoke('save_settings', { serverUrl: $('server').value, token: $('token').value || null, micThreshold: Number($('mic').value) / 600, pc, autoUpdate: $('autoUpdate').checked });
      closeSettings();
    } catch (err) {
      $('error').textContent = String(err);
      $('error').hidden = false;
    } finally {
      save.disabled = false;
      save.textContent = 'Save';
    }
  });

  // Not set up yet: straight to the connection settings.
  invoke('get_settings').then((s) => {
    if (!s.paired || !s.server_url) openSettings(true);
  });
})();
