# The PC app (`crates/desktop`) internals

`crates/desktop` (package `adrian-desktop`, version 0.17.1) is the Windows app people
install as `ADRIAN.exe`. It is a Tauri 2 tray app (WebView2 for the window) that keeps a
microphone open, sends speech to the A.D.R.I.A.N. bot server, plays back what the server
sends, shows an animated orb, runs PC actions through [`crates/pc-control`](pc-control.md),
tells the server when a game is in front, rings alarms by itself if the server can't, and
updates itself from GitHub releases.

It is a client of the bot repo
([adrian-discord-voice](https://github.com/Riqqqque/adrian-discord-voice)), not of the
older Rust server in this repo. The bot does everything that costs money (speech
recognition, the language model, the voice); the PC app calls no AI service itself. See
[architecture.md](architecture.md) for how the pieces fit, and the bot's
[pc-app.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/pc-app.md) for
installing and pairing from a user's point of view.

Numbers on this page are code constants unless they are marked as measured.

This page describes the code on the branch that combines the 2026-11-20 improvement passes
(the crate is still 0.17.1). What those passes changed reaches installed PCs with **the next
PC app release**; PCs on 0.17.1 or older keep the old behavior until they update. The list is
in [Changes in the next release](#changes-in-the-next-release).

## Contents

- [Changes in the next release](#changes-in-the-next-release)
- [Files](#files)
- [Process, threads and tasks](#process-threads-and-tasks)
- [Command-line flags and start-up](#command-line-flags-and-start-up)
- [Window, tray and orb](#window-tray-and-orb)
- [Listening: the mic gate](#listening-the-mic-gate)
- [Playback and audio devices](#playback-and-audio-devices)
- [The link to the server](#the-link-to-the-server)
- [PC actions](#pc-actions)
- [Quiet while gaming](#quiet-while-gaming)
- [Alarms: the backup ring](#alarms-the-backup-ring)
- [Self-update](#self-update)
- [Quiet mode in the tray](#quiet-mode-in-the-tray)
- [Settings](#settings)
- [Files on disk](#files-on-disk)
- [Installer](#installer)
- [Footprint (measured)](#footprint-measured)
- [Invariants and the tests that guard them](#invariants-and-the-tests-that-guard-them)
- [Known issues (open)](#known-issues-open)
- [Known limitations](#known-limitations)
- [Change guide](#change-guide)

## Changes in the next release

Made and unit-tested on 2026-11-20, not yet released, installed or tried on a live server.
Details are in the sections linked.

| Area | Change |
|---|---|
| [Orb](#window-tray-and-orb) | Draws at most about 60 times a second on any refresh rate; motion is timed by the clock, so it looks the same on 60, 120 or 480 Hz screens. Measured in headless Edge: about 40 % less CPU on a 120 Hz screen |
| [Playback](#playback-and-audio-devices) | The speaker callback no longer plays a silent buffer when a reply is being queued; `playing()` no longer takes the queue lock. A device switch keeps what she was saying (resampled to the new rate). Speakers back after an outage longer than 1.5 s play only what arrived in the last 1.5 s |
| [Output kind](#what-plays-her-calls-here-headphones-or-speakers) | The app tells the server whether her calls play on headphones or speakers, from Windows' form factor or a new setting, "These speakers are headphones" |
| [Push-to-talk](#listening-the-mic-gate) | Held until key-up: the app now sends the release, and a new server takes only what was said between press and release |
| [Notice chime](#the-link-to-the-server) | A soft two-note chime when the server heard something at this PC and left it alone on purpose |
| [Reconnect](#reconnect-schedule) | Back about a second after a server restart instead of several (first try after 250 ms, then once a second for 30 s, then a ramp to 10 s); long outages are tried as often as before |
| [link.log](#linklog) | A small local log of how each link ended and why tries failed, with the server's name and the token blanked |
| [Alarms](#alarms-the-backup-ring) | An alarm answered here goes to the server at once on a live link. Answers survive a stale list from the server. A local ring survives a resync. Fixes for a snooze after a dropped link, a clock that jumps ahead, and sleep in the middle of a ring |
| [Tests](#invariants-and-the-tests-that-guard-them) | Desktop tests 73 → 116 attributes (113 pass, 3 ignored measurements); a CI workflow ([CONTRIBUTING.md](../CONTRIBUTING.md#continuous-integration)) and the orb bench (`bench/`) |

Known issues that these passes found and did not fix are listed under
[Known issues (open)](#known-issues-open).

## Files

| Path (under `crates/desktop/`) | Lines | Role |
|---|---|---|
| `Cargo.toml` | 36 | Manifest. Tauri pinned `=2.11.6` (feature `tray-icon`), build-dep `tauri-build = "=2.6.3"`. Also `tokio`, `tokio-tungstenite` (native roots TLS), `cpal 0.16`, `global-hotkey`, `directories`, `rustls` (aws-lc-rs), `ureq` (updates), `sha2`, `base64`, `windows 0.58` + `windows-core 0.58` |
| `build.rs` | 3 | `tauri_build::build()` |
| `tauri.conf.json` | 39 | Window `main`: 460x600 (min 340x420), frameless, shadow, resizable, centered, background `#06080b`, `visible: false`, `create: false`. CSP below. `bundle.active: false`. `version` must equal `Cargo.toml` |
| `capabilities/default.json` | 16 | Window permissions: `core:default`, start dragging, minimize, hide, toggle maximize, close |
| `gen/schemas/` | | Generated by `tauri-build`, committed |
| `icons/` | | `icon.ico`, `icon.png`, `tray.png` |
| `src/main.rs` | 971 | Entry point, flags, Tauri setup, tray, push-to-talk hotkey, show/close, the frame thread (orb state, quiet mode, minimized-to-tray, device changes, game check), the alarm thread, the update thread, the single-instance listener, all Tauri commands |
| `src/link.rs` | 2105 | WebSocket client: reconnect loop `run`, one connection `session`, the reconnect schedule (`Retry`, `retry_wait`, `short_wait`), refusals `refused`/`closed`, send with time limit `put`, `action_result`, input gating `offer` / `talk_key` / `talk_hotkey`, alarm answers `answer_alarm` / `send_done`, the notice chime, what goes into link.log (`Trace`, `Away`, `kind_of`, `host_of`). About two thirds of it is tests, including the outage harness |
| `src/linklog.rs` | 116 | link.log: two files of at most 128 KB, words blanked, UTC timestamps |
| `src/audio.rs` | 1340 | Mic capture, the gate (`Gate`), echo-aware barge-in, playback queue (`fill`, the outage marks, `drop_stale`, `retarget`), speaker idling, default-device following and the output's form factor (`mod watch`), `output_kind`, audio thread priority (`audio_thread`) |
| `src/alarm.rs` | 782 | Alarm copies and local ringing (`Alarms`, answer markers `Answered`), the local alarm sound (`Beeper`) |
| `src/update.rs` | 678 | Self-update from GitHub releases |
| `src/config.rs` | 503 | Settings load/save, all-or-nothing `commit`, `normalize_url`, shortcut text format, DPAPI token storage, the headphones marks (`mark_headphones`) |
| `src/quiet.rs` | 74 | Background priority, EcoQoS and working-set trim |
| `src/single.rs` | 102 | Single instance through a named event |
| `src/tray.rs` | 160 | Tray tooltip text and the warning dot on the icon |
| `ui/index.html`, `ui/app.js`, `ui/style.css` | 126 / 274 / 140 | The window: title bar with status chip, alarm panel, transcript, typing box, hold-to-talk, settings sheet |
| `ui/orb.js` | 350 | Canvas orb renderer `createOrb(canvas, opts)`, with its own frame pacing |
| `bench/orb-bench.mjs`, `orb-bench.html`, `empty-orb.js` | 207 / 175 / 15 | The orb bench: times `orb.js` in headless Edge under a fake vsync, or compares two versions' pixels. Not part of the app or of any test run ([Change guide](#change-guide)) |
| `packaging/Install.ps1` | 75 | Per-user installer |
| `examples/aec_probe.rs` | 270 | Manual echo-cancellation experiment (plays a WAV, records the mic raw and as a Windows "communications" stream, reports levels). Not used by the app |

The `ui/` folder is embedded in the exe at build time (`build.frontendDist: "ui"`); rebuild
to see UI changes. `crates/satellite/ui/orb.js` is an older, different copy and is not used.

## Process, threads and tasks


One process. Release builds have no console (`windows_subsystem = "windows"`), so `println!`
and `eprintln!` output is not visible; scripts must use exit codes.

| Thread or task | Started in | Does |
|---|---|---|
| Main (Tauri event loop) | `main` | Window, tray, menu events, Tauri commands. Tray and window changes are made here |
| `adrian-gate` | `audio::start` | Takes mic samples, resamples to 48 kHz, cuts 20 ms frames, runs the gate and echo estimate, queues frames for the link, sets the duck flag |
| `adrian-audio` | `audio::start` | Device thread: opens and reopens the mic and speaker streams (cpal), pauses idle speakers, handles `Ctl::Failed` / `Wake` / `DefaultChanged`, retries failures |
| `adrian-devices` | `audio::watch::start` | COM MTA thread holding an `IMMNotificationClient` for default-device changes |
| cpal stream threads | cpal | Mic and speaker callbacks. Each registers with MMCSS ("Audio") and opts out of EcoQoS on its first callback (`audio_thread`) |
| Link task | `tauri::async_runtime::spawn(link::run(...))` | The WebSocket connection, reconnects, all messages both ways |
| Action workers | `tokio::task::spawn_blocking` in `link.rs` | One per PC action, running `Pc::run` |
| Event task | async task in `main` | Turns link events into window events and tray updates |
| Frame thread | `std::thread::spawn` in `main` | Every 50 ms with a window, every 500 ms without: orb state to the page, orb pause/resume, quiet mode, minimized-to-tray, device line, game check every 2 s |
| Alarm thread | `std::thread::spawn` in `main` | Every 200 ms: `Alarms::tick`, local beeps, speaker raise/restore, alarm window and tray items |
| Update thread | `std::thread::spawn` in `main` | `update::run`: checks, downloads, waits for idle, installs |
| Hotkey thread | `std::thread::spawn` in `main` | Ctrl+Alt+Space presses and releases |
| Show listener | `std::thread::spawn` in `main` | Waits on the single-instance event and shows the window |

Shared state between them is `audio::Shared` (atomics and mutexes) and Tauri managed state
(`AppState`: settings, status, the current `Pc`).

## Command-line flags and start-up

| Flag | Effect |
|---|---|
| `--pair SERVER TOKEN` | Saves the server address and token and exits: 0 and `Paired with <url>.` on success, 1 on bad input. The address must pass `normalize_url` and the token must start with `sat_`. Replaces any existing pairing |
| `--background` | Start in the tray without showing the window. The window still opens if the app is not paired |
| `--update-now` | Check, download, verify and install the newest release at once (no idle wait). Exit 1 on failure |
| `--self-check` | Build the Tauri app without a window, tray, audio, single-instance claim or file access, print the version, exit 0 (1 if Tauri can't build). The updater uses it to prove a download starts |
| `--after-update` | Used by the updater: retries the single-instance claim 40 times every 250 ms (10 s) while the old copy exits, and does not wake a running copy |

Start-up order, in short: claim single instance (a second launch signals the running copy
to show its window and exits; `--after-update` retries the claim); delete a leftover
`ADRIAN.new.exe`; install the rustls aws-lc-rs provider; load settings; register the hotkey; load `alarms.json` and restore speakers
an alarm raised before a crash; start audio and the link; start the frame, alarm,
update and show threads.

## Window, tray and orb

- **No window in the tray.** `tauri.conf.json` has `create: false`; `show()` builds the
  window from config (`WebviewWindowBuilder::from_config`), and closing it (X, tray click,
  or 2 minutes minimized, `MINIMIZED_TO_TRAY` = 120 s) destroys it, so the WebView2
  processes exit. `RunEvent::ExitRequested { code: None }` is prevented so closing the last
  window doesn't quit; the tray's Quit calls `app.exit(0)`.
- **Tray.** Tooltip "A.D.R.I.A.N." + status line + mic error (cut to 127 UTF-16 units,
  `TIP_LIMIT`). An orange dot is drawn on the icon when not connected or when audio
  devices failed. Left click toggles the window. Menu: Show A.D.R.I.A.N., Stop alarm,
  Snooze alarm (both enabled only while an alarm rings), Quit.
- **Window.** Status chip (Ready, Listening, Thinking, Speaking; Connecting or Offline when
  not connected; "No microphone" while idle when the mic or the speakers failed, since
  every audio device error is stored in one `mic_error`; the full error is in the tray
  tooltip), transcript (last "you" and "her"
  lines, her line grows while the server streams it, both fade 20 s after her last line),
  typing box (2000 characters; refused offline with "Not sent, she's offline." and kept in
  the box), hold-to-talk button, stop button, settings sheet, alarm panel. Escape closes
  settings, otherwise sends stop. Hint line: Say "Hey Adrian", hold Ctrl Alt Space, or type.
- **CSP** (`tauri.conf.json`): `default-src 'self'; script-src 'self'; style-src 'self'
  'unsafe-inline'; img-src 'self' data:; connect-src ipc: http://ipc.localhost`. No inline
  scripts, no remote content.
- **Tauri commands** (`generate_handler!`): `get_settings`, `get_status`, `save_settings`,
  `send_text`, `stop_talking`, `push_to_talk`, `her_state`, `orb_drawing`, `audio_devices`,
  `alarm_action`, `alarm_state`.
- **Orb** (`ui/orb.js`): `createOrb(canvas, opts)` returns `setMode`, `setLevel`,
  `setAccent`, `lean`, `setActive`, `destroy`. 23 rings, fixed tilt 0.42, 230 dust
  particles, bloom from a half-size blurred layer (`ctx.filter`, so it needs
  Chromium/WebView2). The accent is fixed at `#9fd4ff` in `app.js`; `lean` exists but
  nothing calls it.
- **Orb pacing** (next release). The orb draws at most about 60 times a second (`MAX_FPS`)
  whatever the monitor's refresh. It learns the refresh rate from `requestAnimationFrame`
  timestamps, counting only gaps after a quick callback (a drawing that ran past a refresh
  would make the screen look slower), and draws every n-th callback, n = floor(refresh / 60
  + 0.25), moving n only when clearly past a boundary. That gives 60 fps at 120, 240 and
  480 Hz, 72 at 144 Hz, every frame at 60, 75 and 90 Hz, and settles once on 55 or 82 at
  165 Hz. Motion and easing are timed by the clock and scaled to how the look was tuned
  frame by frame on a 120 Hz screen (`TUNED_HZ`), so the orb looks the same at any rate. If
  it feels softer on a very fast screen, raise `TUNED_HZ`. Before, it drew on every refresh
  and its per-frame easing ran faster on faster screens.
- **Orb cost (measured 2026-11-20, headless Edge, not the app's WebView2 window).** The
  app's window size (460x600 CSS px at 125 %), a fake 120 Hz vsync and a 20 s script through
  all four states; 5 interleaved runs of 15 s, medians:

  | 120 Hz screen | before | after |
  |---|---|---|
  | Frames drawn per second | 101.8 (the harness couldn't reach 120) | 61.3 |
  | Script per drawn frame | 4.40 ms | 4.01 ms |
  | Script per second | 448 ms | 246 ms |
  | Edge CPU, all processes, per second | 990 ms | 608 ms (-39 %) |
  | JS heap | 1.54 MB | 1.26 MB |

  CPU per drawn frame stayed the same (about 9.5 ms), so the saving comes from drawing half
  as often; on a real 120 Hz screen, where the old orb drew all 120 frames, it is closer to
  50 %. At 60 Hz nothing got worse (script per frame 4.66 → 4.10 ms). Pixels compared every
  0.5 s on one synthetic clock differ less between the new and old orb at 120 Hz (mean under
  1 of 255 per channel) than the old orb already differed between a 60 and a 120 Hz screen,
  and the new orb draws exactly the same pixels at 120 and 480 Hz. The ~1550 ring-point
  `drawImage` calls are most of the script time; the bloom is a small share. Absolute
  numbers in WebView2 will differ; the app itself has not been measured with the window open.
- **Orb states** (frame thread):
  - `speaking`: audio is playing and it is her voice (server `state.speaking`; older
    servers: while answering). Music alone does not make the orb speak.
  - `listening`: online and either push-to-talk is held, or the gate is open while she
    would take words without her name (server `state.listenMs`; older servers: within 20 s
    after her voice). A loud noise alone doesn't light it. Never while offline.
  - `thinking`: server `mode: "thinking"`.
  - `idle`: everything else.
- **Orb pause.** WebView2 does not set `document.hidden` for a hidden Tauri window, so the
  app pauses the orb itself. It draws only while the window is visible, not minimized, and
  either focused or with no full-screen app in front (`orb_on_screen`; full screen means
  `SHQueryUserNotificationState` returns `QUNS_BUSY`, `QUNS_RUNNING_D3D_FULL_SCREEN` or
  `QUNS_PRESENTATION_MODE`). The frame thread emits `draw` true/false and, while paused,
  sends only state changes (checked every 500 ms) instead of 20 frames a second (`Pacer`).

## Listening: the mic gate

The mic is always open on the Windows default input device. Audio leaves the PC only while
the gate is open: `speech_start`, then binary frames, then `speech_end`. The server's own
VAD and wake-phrase check decide what is really for her.

| Constant (`audio.rs`) | Value | Meaning |
|---|---|---|
| `RATE` / `FRAME` | 48 000 Hz / 960 samples | 20 ms mono frames, sent as s16le |
| `PREROLL_FRAMES` | 15 (300 ms) | Audio from before the gate opened, sent first |
| `OPEN_FRAMES` | 3 (60 ms) | Frames above the threshold to open |
| `CLOSE_FRAMES` | 60 (1.2 s) | Quiet frames to close |
| `MAX_FRAMES` | 1500 (30 s) | Longest single stretch |
| `BARGE_FRAMES` | 4 (80 ms) | Frames to open over her voice |
| `BARGE_OVER_ECHO` | 3.0 | How much louder than her expected echo you must be |
| `DUCK` | 0.3 | Her playback gain while you talk over her |
| `ECHO_TAIL_MS` | 350 | Her voice counts as playing this long after it stops |
| `UP_QUEUE` | 1500 frames (30 s, about 3 MB) | Bound on audio waiting for the link |

- **Automatic threshold.** A background floor starts at RMS 0.003, follows quieter frames
  quickly (k 0.1) and louder ones slowly (k 0.002), clamped to 0.0005..0.02, and is never
  updated while the gate is open, under push-to-talk, or while echo is expected. The
  threshold is floor x 3, clamped to 0.012..0.035, so speech or a show can't teach it the
  room is loud and leave it deaf. A manual threshold from settings overrides it.
- **Full duplex.** While anything plays, the expected echo is a learned coupling x the
  loudest speaker RMS of the last 240 ms (12 frames). The coupling starts at 1.0, learns only
  while playing with speaker RMS above 0.004 and the gate shut (k 0.05 down, 0.01 up), and is
  clamped to 0.02..2.0. To open over her, the mic must exceed max(threshold, echo x 3 + 0.01)
  for 80 ms. While open, a frame is quiet below max(threshold x 0.7, echo x 1.5). There is no
  acoustic echo canceller; a 2026-10-23 probe found her echo far below real speech at a desk
  mic, so the learned coupling is enough there.
- **Ducking.** When the gate opens while her voice plays, the PC drops her to 30 % at once;
  the server stops her a moment later. Music alone is never ducked for a noise.
- **Push-to-talk** (Ctrl+Alt+Space held, or the hold-to-talk button) forces the gate open
  while held. On press it sends `{"type":"ptt","down":true,"hold":true}`, and on release
  `{"type":"ptt","down":false}` (next release; 0.17.1 sends only the press, without `hold`).
  The release is sent only while online: offline there is no press to end. After release
  the gate closes after 1.2 s of quiet as before. Push-to-talk never survives a lost link;
  only a hotkey still physically held resumes it after a reconnect (as a new press).
- **What the server makes of it.** With `hold`, a new bot takes everything said between the
  press and the release as push-to-talk, and nothing said after the release: **hold the key
  while you talk**. Tapping it and then talking no longer works. Speech already going at the
  press doesn't count, and words in the last ~150 ms before letting go can be clipped if the
  audio arrives late. A key-up that never comes ends the press after 60 s. Without `hold`
  (0.17.1 and older apps) the bot keeps its old one-shot press: the next thing heard within
  15 s. An older bot ignores both `hold` and the release, so a new app works there as today.
  The server side is in the bot's
  [satellite.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md).
- **Offline.** Frames go into a bounded channel with `try_send`; nothing waits. With no
  link they are dropped, and the link drains the queue on every new connection, so audio
  from while offline is never sent.

## Playback and audio devices

- Server audio (48 kHz mono s16le) is resampled to the output device rate and queued; the
  queue holds at most 8 s (`MAX_BUFFER_SECS`), oldest dropped. `audio_stop` from the server,
  the stop button or Escape clear it.
- Speakers start paused and pause again after 3 s of silence (`SPEAKER_IDLE`), so the app
  doesn't feed silence to the audio engine all day; new audio wakes them (`Ctl::Wake`).
- **The speaker callback and the queue lock** (next release). The output callback waits up to
  2 ms (`FILL_WAIT`) for a reply being queued, instead of playing a silent buffer whenever
  the lock was busy (it used `try_lock`), and `playing()` reads an atomic count instead of
  taking the lock. Pushing a reply holds the lock for microseconds, so the wait is normally
  zero.
- **Default device following.** `IMMNotificationClient::OnDefaultDeviceChanged` for role
  `eConsole` sends `Ctl::DefaultChanged`; the device thread waits a fixed 250 ms (a burst of
  notifications arrives per switch), then reopens both streams on the new defaults. Since the
  next release, what she was saying carries on there: the queue is kept and resampled if the
  new device runs at another rate (0.17.1 dropped audio queued at the moment of a switch). The
  settings line shows the device names.
- **Failures** (unplugged, busy): an error in the tray and window ("No microphone found.",
  "No speakers found.", "Microphone: ...", "Speakers: ..."), retry after 300 ms doubling to 5 s.
- **Stale speech after an outage** (next release). Speakers gone **longer than 1.5 s**
  (`STALE`; counted from the first failure, failed retries don't restart it): when they come
  back, only audio queued in the last 1.5 s is kept, so an answer that ended while nothing
  could play it isn't played late. Before, up to the full 8 s queue played. A dropout of
  1.5 s or less, or a device switch where nothing failed, keeps everything. While an alarm
  rings nothing is dropped, because the PC's own beeps are queued about a second ahead. The
  cut runs before every try at opening the speakers, so a queue that can't play no longer
  keeps `playing()` true (which used to hold up quiet mode and the updater's idle check).
  This relies on the server sending audio at real-time pace, as the bot's mixer does; a
  burst sent faster than real time during an outage would be cut to its last 1.5 s.

### What plays her calls here: headphones or speakers

Next release. The bot uses this for its call guard: while the PC's owner is in a Discord
voice call the bot can see, and this PC says its output is **speakers** (or can't tell), the PC takes only
push-to-talk and typed text, because its mic hears the call too. A PC on **headphones** stays
hands-free. The rule itself, and its exceptions, are the bot's: see its
[satellite.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md).

- After each stream open, `watch::default_output_form()` reads the default console render
  endpoint's `PKEY_AudioEndpoint_FormFactor` (read-only COM).
- `output_kind(form, marked)`: marked in settings, or form factor 3 (Headphones) or 5
  (Headset), is `headphones`; 10 (Unknown) or unreadable is `unknown`; anything else is
  `speakers`. The server treats `unknown` like speakers.
- The app sends `{"type":"output","kind":...}` right after every welcome and again whenever
  the kind changes (a device switch or a Save). For the moment between the welcome and that
  message the server counts the PC as unknown, so guarded.
- **Settings: "These speakers are headphones"**, under the device line. It marks the output
  in use when Save is pressed, by its Windows name (`headphones` in `config.json`), and only
  the kind goes to the server, never the name. The switch is ticked when the output counts
  as headphones and disabled when Windows itself already says so (or when there are no
  speakers).
- **When to tick it.** Many USB DACs, docks and audio interfaces report "Speakers" whatever
  is plugged into them, so earphones or headphones on one of those look like speakers. Tick
  it once with that output as the Windows default, then Save. Until then, hands-free stays
  off at this PC while you're in a voice call. Don't tick it for real speakers: the mark is
  trust in the user and turns the guard off for that PC. A renamed device, or the same DAC
  later used with speakers, needs the mark changed.
- **Priority.** cpal 0.16 passes a thread id where Windows wants a handle, so its own priority
  boost never worked. `audio_thread()` registers each stream thread with MMCSS "Audio" and
  opts it out of execution-speed throttling, so quiet mode never slows the mic or speakers.

## The link to the server

The wire contract is owned by the bot: [satellite.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md).
This is what this client implements (`link.rs`).

**Address.** `normalize_url` trims, strips a trailing `/`, adds `ws://` when there is no
scheme, maps `http://` to `ws://` and `https://` to `wss://`, and adds `/satellite` when
there is no path. `myserver:8791` becomes `ws://myserver:8791/satellite`. Whitespace inside
is invalid. `wss://` uses the Windows certificate store (native roots).

**PC to server**

| Message | When |
|---|---|
| `hello {token, device, version}` | First frame. `device` is `%COMPUTERNAME%` (or `PC`), `version` is `CARGO_PKG_VERSION`. The bot ignores `device` and uses the paired name |
| `speech_start`, binary PCM, `speech_end` | While the gate is open |
| `text {text}` | Typed text (2000 characters max) |
| `stop` | Stop button or Escape |
| `ptt {down: true, hold: true}` | Push-to-talk pressed. `hold` (next release) says a key-up will follow |
| `ptt {down: false}` | Push-to-talk released (next release; only while online) |
| `output {kind}` | `headphones`, `speakers` or `unknown`: what plays her calls here. After every welcome and on change (next release) |
| `alarm_action {action}` | Stop or Snooze for an alarm the server is ringing |
| `alarm_done {id, base, action, at}` | An alarm answered while the PC rang it itself. Next release: sent at once on a live link; answers made offline go out after the next welcome. 0.17.1 sent them only after a welcome |
| `activity {gaming, game?}` | Game in front changed (see below); repeated after each welcome while gaming |
| `action_result {id, ok, result, image?}` | Answer to an `action`; `image {mime, data}` (base64) for pictures |
| `ping` | Every 20 s, the first right after connecting |

Before `welcome` the PC sends only `hello`, `ping`, and `action_result` refusals ("Not
connected yet."). Speech, text, stop, ptt, output, activity and alarm answers wait for the welcome,
and anything typed, pressed or recorded while offline is dropped, never sent later: a stale
"lock the PC" must not run minutes later.

**Server to PC**

| Message | Effect |
|---|---|
| `welcome {name, owner}` | Online. Status "Connected as <name>". Then sends `ptt` if the hotkey is held, `output`, `activity` if gaming, and pending `alarm_done`s. `owner` is kept but not shown |
| `denied {reason}` | Refusal (except `no hello`, which counts as a slow server) |
| `mode {mode}` | `thinking` or `idle` |
| `state {speaking, listenMs}` | Her voice audible; how long she takes words without her name. Its presence marks a newer server (`has_state`) |
| `transcript {who, text, final}` | Window transcript |
| binary | Her audio, music and sounds (48 kHz mono s16le) |
| `audio_stop` | Clear the playback queue |
| `notice {kind}` | Next release, accepted only after welcome. The server heard something here and left it alone on purpose (today `kind: "ptt_only"`: her name said at a PC that takes only push-to-talk). The app plays a soft chime through its own playback queue: 880 Hz for 0.12 s then 660 Hz for 0.18 s, soft sines at no more than 12 % of full scale, 0.3 s in all. Nothing pops over a game (Windows hides toasts in full-screen games). The bot sends at most one per PC every 20 s; a loud game or a ringing alarm can mask it |
| `alarms {...}` | Alarm state (accepted only after welcome) |
| `action {id, name, args}` | Run a PC action (refused before welcome) |
| `pong` | The bot's answer to `ping`. Like any message it resets the unanswered-ping count, so a bot that stops answering pings is treated as a lost link |
| anything else | Ignored, but resets the unanswered-ping count. Unknown types are safe for old apps |
| close frame | Before welcome, code 1008 counts as a refusal (the server's address lockout closes this way without `denied`) |

**Timing**

| Item | Value |
|---|---|
| Connect timeout | 8 s |
| Any single send (`SEND_LIMIT`) | 10 s; a stuck send means the link is lost |
| JSON ping (`PING`) | every 20 s |
| Lost link | no message for the third ping tick after the last one (`UNANSWERED` = 2), i.e. 40 to 60 s. Server WebSocket pings count as messages |
| Reconnect waits | See [Reconnect schedule](#reconnect-schedule). 0.17.1: 1, 2, 5, then 10 s |
| Backoff reset (`HELD`) | only after a welcomed connection lasted 30 s, so two PCs on one token back off instead of replacing each other every second |
| After a refusal (`REFUSED_RETRY`) | retry every 5 minutes; a settings change reconnects at once |

On every disconnect: online off, push-to-talk released, playback cleared, listen state and
thinking cleared; alarms the server was ringing keep ringing locally.

**Compatibility of the next release's messages.** All are optional additions. A new app
with an older bot works as today: the bot ignores `output` and `hold`, already ignored
`ptt {down: false}`, accepts `alarm_done` at any time on a link, and never sends `notice`.
An older app with a new bot: it never says `output`, so the bot treats it as speakers
(guarded while its owner is in a call); it sends no `hold`, so push-to-talk uses the old
one-shot press; it ignores `notice` (the transcript line still arrives).

### Reconnect schedule

Next release (`Retry` in `link.rs`). The waits follow how long the server has been out of
reach, counted from the end of the last connection that held (or from start-up):

| Since the server went | Wait before the next try |
|---|---|
| first try after a link that held | 250 ms (`FIRST_TRY`) |
| under 30 s (`QUICK_FOR`; a restart takes 20-25 s) | 1 s (`QUICK_WAIT`) |
| 30 to 60 s | ramps from 1 s up to 10 s |
| from 60 s on | 10 s (`LONGEST`), as before |

- A server that answers but doesn't keep the connection (dropped soon after the welcome,
  never welcomed, or "no hello") still gets the old 1, 2, 5, 10 s count on top
  (`short_wait`). That protects two PCs on one token, and the bot's lockout, which counts
  "no hello" as a failure. Refusals still wait 5 minutes.
- The schedule is keyed on whether the last link held, not on the close code: the bot sends
  1000 "bye" both when it shuts down and when it replaces a connection.
- A single connect attempt may take up to 8 s; on Windows a refused connect takes about 2 s,
  because Windows retries the SYN.
- **Measured 2026-11-20** with a model of the schedule over outages from 0.1 to 120 s, which
  is the fair comparison (a single outage length lands on one fixed phase of the schedule).
  Extra wait after the server is back, for 15-30 s outages (restarts), median / worst:

  | Outage shape | Before | After |
  |---|---|---|
  | refused, 2 s per try | 6.7 / 11.9 s | 1.45 / 2.95 s |
  | instant refusal | 4.5 / 10.0 s | 0.5 / 1.0 s |
  | dead path, 8 s per try | 7.5 / 17.9 s | 4.25 / 10.5 s |

  The model is pessimistic for refused tries. A harness that runs the real `run()` loop
  against a local WebSocket server came back 0.33 to 0.48 s after a 21 s refused outage
  (before: about 3 s). From one minute on the tries per minute are the same as before or
  fewer. The cost is more tries in the first minute: 16 refused connects instead of 7, at
  most about 39 with instant refusals; these are TCP connects, not hellos, so they don't
  count toward the lockout.
- A PC frozen past the server's keep-alive (1.5 s in the test) is back about 0.25 s after it
  wakes (test: 251 / 254 / 271 ms min / median / max over 50 runs; 1 s with the old first wait).
- Not tried against a real server restart yet.

### link.log

Next release (`linklog.rs`). A short local record of the link, in the settings folder beside
`alarms.json`: `link.log`, moved to `link.old.log` once it passes 128 KB, so at most two
files. Lines start with a UTC time. It records only link events, a few lines per connection:

- `link ended: <how>; lasted N s, heard from X s before, K ping(s) unanswered, longest hold-up L ms; quiet mode on|off, game in front|no`
- `can't reach the server: <error> (after T ms); ...`, for the first failed try only
- `link back after X s: N failed tries (refused 3, timed out 2, ...; slowest S ms), last: <error>; ...`, or `link back after X s, at the first try; ...`, or `link up; ...`

"Longest hold-up" is the longest wait for any event beyond the 20 s ping timer, which fires at
least that often, so a larger number means the app wasn't run (a starved or suspended
process). Failure kinds come from the error: refused, timed out, unreachable, name not found,
reset, tls, not a satellite server, other; a connection the server took and then closed or
denied is "turned away".

**What's kept out.** The server's host name and the token are replaced by `<server>` in every
line, library error texts included. No words, transcripts, device names or user names are
written. Other text in an error is logged as the library gives it, which has a gap: a TLS
certificate-name error lists every name and address on the server's certificate (see
[Known issues](#known-issues-open)). Check the file before sharing it.

It was added to explain links that dropped as a game started and came back after more than a
minute. Read it next to the bot's `satellite disconnected` line: a starved PC shows a reset
with a hold-up of tens of seconds and quiet mode on, then "back at the first try"; a dead
network path shows "it stopped answering" with 2 pings unanswered and a hold-up near 0, then
timed-out tries; a server restart shows "the server closed it (1000 bye)" and refused tries.
The cause of those drops is still open.

## PC actions

An `action {id, name, args}` runs `Pc::run(name, args)` on a blocking thread with the
current `Pc` (rebuilt on every settings Save, so a changed policy applies to the next
action). The answer is `action_result`. An action before `welcome` gets `ok: false`,
"Not connected yet." The nine tools, the policy and every safety rule are in
[pc-control.md](pc-control.md). The server cannot change the policy.

## Quiet while gaming

- Every 2 s (`GAME_CHECK`) the frame thread calls
  `Pc::game_in_front(fullscreen_app_in_front())` and feeds `activity::Debounce`: playing
  starts after 5 s in a game and ends after 20 s out of one.
- A change is sent as `{"type":"activity","gaming":bool,"game":"<name>"}` (`game` only
  while gaming and known, at most 60 characters).
- Settings switch "Quiet while I play" (stored as `quiet_off`, default off = quiet mode on).
  Turning it off resets to not playing at once.
- What the bot does with it (bot side): answers only to her name, keeps answers short,
  holds reminders until the match ends; timers still ring.
- How a game is recognized: [pc-control.md](pc-control.md#game-detection-quiet-while-gaming).

## Alarms: the backup ring

The bot owns alarms and timers. The PC keeps a copy so an alarm still rings at the PC if the
server or the link is down (`alarm.rs`, alarm thread in `main.rs`).

- **Input.** The server's `alarms` message: `ringing` (what the server rings now; its sound
  arrives as normal audio), `list` (upcoming), `snoozeMin` (1..60, default 9), `ringMin`
  (1..30, default 10), `now` (server clock; the skew is applied to every due time). At most
  64 entries are kept from each list (`MAX_KEPT`), strings are cut to 80 characters, and the
  list's `sound` field is ignored (the PC always beeps). The bot currently sends the next
  8 days; the app itself has no horizon.
- **Ringing locally.** Every 200 ms. An alarm in `list` that is 20 s past due (`GRACE_MS`)
  and that the server isn't ringing rings on the PC, whether or not the link is up, for up to
  `ringMin` minutes, each occurrence at most once. If the link drops while the server rings
  an alarm, the PC keeps ringing it (next release: also when another alarm was already
  ringing here by itself), for only what is left of its ring time: `server_said`
  remembers when each was first seen ringing. 0.17.1 started it over for a full ring time,
  so a PC waking from sleep hours later could ring a morning alarm at noon for 10 minutes.
- **The server wins, with care** (next release). A new `alarms` message used to stop any
  local ring. Now a local ring survives the server's hourly or on-change resync and stops
  once the server rings it itself, lists a later time or another occurrence, or drops it.
  One exception: a local ring that isn't due by the server's clock in that message (`at +
  GRACE_MS > now`) stops and comes out of the already-rung set, so it rings at its real time. That is the case
  of a PC clock that jumped ahead and rang an alarm early; the skew is refreshed only by the
  server's lists.
- **Sound.** `Beeper`: a soft 1 kHz square-ish tone (fundamental + 3rd + 5th harmonics),
  four 70 ms beeps 125 ms apart, then a pause, a 1.6 s cycle at amplitude 0.3, pushed through
  the normal playback queue about 1 s ahead.
- **Speakers.** While any alarm rings, a master volume under 30 % (`FLOOR`) or muted is
  raised to at least 30 % and unmuted. The old level is saved to `alarms.json` first and put
  back when ringing ends, or at the next start after a crash.
- **Stop and Snooze** (window panel, tray menu): for a server ring, `alarm_action` goes up.
  For a local ring, the PC stops it and records `alarm_done`. Next release: it goes to the
  server at once if the link is up (`link::answer_alarm`, `Cmd::AlarmDone`); otherwise after
  the next welcome. Either way it is removed from `done` once it has been written to the
  socket. A local snooze moves the copy to now + `snoozeMin`. A snooze after the link
  dropped mid-ring now rings again.
- **Answers kept against a stale list** (next release). Every answer given here leaves a
  marker `{id, base, answered, until}` in `alarms.json` (`until` is the snooze time, none for
  a stop). A list entry with the same id and base and `at <= answered` was made before the
  server heard the answer (the hourly resync crossing the `alarm_done`, or the list the bot
  sends right after the welcome, before the PC's pending answers go out), so the PC keeps its
  own answer: a snoozed copy keeps `until`, a stopped one stays out. The server wins as soon
  as it lists a later time, another occurrence, or nothing. Without this, a stale list put
  the old time back, and after a restart the PC rang the old occurrence again. Markers are
  capped at 64; files from 0.17.1 load (`#[serde(default)]`). The snooze length here is the
  list's `snoozeMin`; a per-alarm snooze length the bot keeps isn't in the list, so offline
  a local re-ring can differ from the server's, and online the server's time wins once it
  pushes its list.
- **Clocks.** Every time on the PC is epoch milliseconds and the server computes due times
  in the alarm's own time zone, so daylight saving and time zones don't matter here. A PC
  clock that jumps is handled only when the next list arrives (see the open issues A2 and A6
  below). Sleep through a whole local ring leaves it quiet on waking.
- **Window.** An alarm opens the window without focus, unless a full-screen app is in front.
  If the alarm opened it, it closes again 4 s after the ring ends, unless it has focus.
- Labels: "Alarm: <label>", "<label> timer", "Alarm", "Timer"; Snooze reads "More time" for timers.

## Self-update

`update.rs`. High risk: a bug here can strand every installed PC on a version that can't update.

| Step | Detail |
|---|---|
| Source | `https://api.github.com/repos/<owner>/adrian-discord-voice/releases/latest` (constant `REPO`), unauthenticated, user agent `adrian-desktop/<version>`, global timeout 300 s |
| Schedule | first check 3 min after start (`FIRST_CHECK`), then every 6 h (`EVERY`); turning auto-update on ends the wait for the next 6-hour check at once (in the first 3 minutes after start the first check still waits for `FIRST_CHECK`) |
| Version | tag `v<X.Y.Z>` (leading `v` stripped, only `[A-Za-z0-9.+-]`), compared as dotted numbers (missing parts = 0) |
| Assets | `adrian-pc-app-<tag>-windows-x64.exe` and `.exe.sha256`; both URLs must start with `https://github.com/<REPO>/releases/download/` |
| Download | `.sha256` up to 4096 bytes (first token must be 64 hex characters), exe up to 200 MB; SHA-256 must match |
| Prove | written as `ADRIAN.new.exe` beside the running exe (`sync_all`), run with `--self-check` (30 s limit, no console window); it must exit 0 and print exactly the tag, else it is deleted |
| Wait for idle | checked every 30 s: no window, nothing playing, gate shut, no push-to-talk, not answering, no full-screen app in front, auto-update still on |
| Swap | re-hash the file on disk, hard-link the running exe to `ADRIAN.old.exe`, rename the new one over `ADRIAN.exe` (an app exists at every moment; without hard links, two renames with rollback). Start `--background --after-update`, exit. If the new copy can't be started, the files are swapped back |
| Keep the old copy | `ADRIAN.old.exe` is deleted only once the new version is proven: welcomed by the server and running at least 3 minutes (`proven`). It is the manual rollback path |
| Off | with `updates_off` it only reports that an update is available |

Settings line examples: "Version 0.17.1, up to date.", "Version X. Y is downloaded and
installs the next time she's idle in the tray.", "Version X. The last update check failed: ...".

Rules that follow from the code:

- `Cargo.toml` `version` must equal the release tag (and `tauri.conf.json` must match), or
  `--self-check` fails and the update is refused.
- Every release must carry the bare exe and its `.sha256`, or PCs report that the release has
  no PC app download and never update.
- The first update onto a new release always runs the previous version's updater, so a new
  safety gate protects only the update after the one that ships it.
- Test `--update-now` only on a copy in a temporary folder.

## Quiet mode in the tray

`quiet.rs`, driven by the frame thread every 500 ms when there is no window. While there is
no window and she isn't playing, push-to-talk isn't held and the server isn't answering, the
process runs at BelowNormal priority with EcoQoS (execution speed + ignore timer resolution)
and low memory priority, and its working set is trimmed (`K32EmptyWorkingSet`). Any of those
changing puts it back to normal at once. Audio threads are exempt (see above).

## Settings

File: `%APPDATA%\adrian\desktop\config\config.json`
(`directories::ProjectDirs::from("ai", "adrian", "desktop")`). Pretty JSON, written atomically
(`config.json.tmp` + rename). Every field has `#[serde(default)]`; a missing key takes its
default and a corrupt file loads as all defaults (unpaired).

| Key | Type | Default | Effect | Window control |
|---|---|---|---|---|
| `server_url` | string | `""` | Bot address, normalized as above | Connection: server address |
| `token_protected` | hex string | `""` | The `sat_` token, encrypted with DPAPI (`CryptProtectData`, current Windows user, no extra entropy), stored as hex. Never shown | Connection: pairing token (placeholder "Paired. Paste a new token only to replace it.") |
| `mic_threshold` | number | `0.0` | 0 = automatic gate; otherwise a fixed RMS threshold, clamped 0..0.1 | Microphone: Sensitivity slider 0..60 (value / 600) with a live level meter |
| `pc` | object | everything on, no shortcuts | The PC action policy and shortcuts ([pc-control.md](pc-control.md#the-policy)) | "What she may do on this PC" toggles and the Shortcuts box |
| `updates_off` | bool | `false` | Never install updates, only report them | Updates: "Update automatically" |
| `quiet_off` | bool | `false` | Never send `activity`, so quiet while gaming never applies to this PC | Games: "Quiet while I play" |
| `headphones` | list of strings | `[]` | Next release. Windows names of outputs marked as headphones; the output in use counts as `headphones` for the [call guard](#what-plays-her-calls-here-headphones-or-speakers). Older files have none | Microphone: "These speakers are headphones" (marks or unmarks the output in use at Save; a page without the switch leaves the list alone) |

- **Save is all or nothing** (`config::commit`): an invalid address, a token not starting
  with `sat_`, or an unreadable shortcut line returns an error and changes nothing; a failed
  disk write changes nothing.
- Save reconnects only if the address or token changed, or if not connected (a reconnect
  drops what she's doing). Turning auto-update on cuts the wait for the next check short
  (not during the first 3 minutes after start).
- Shortcut lines: `name = target` or `name = "target" args`, one per line.
- Not paired or no address: the settings sheet opens on load with the connection section expanded.

## Files on disk

| File | Location | Notes |
|---|---|---|
| `config.json` | `%APPDATA%\adrian\desktop\config\` | Settings above |
| `alarms.json` | same folder | `{list, done, answered, snooze_min, ring_min, restore}`; alarm copies (`{id, kind, label, at, base}`, epoch ms), answers waiting to be sent, answer markers (next release; missing in older files), and the saved speaker level while an alarm raised it. Written atomically on every change |
| `link.log`, `link.old.log` | same folder | Next release. [link.log](#linklog): at most two files of about 128 KB |
| `ADRIAN.exe` | install folder | The app |
| `ADRIAN.new.exe` | beside the exe | Verified download waiting to install; deleted at start by the copy that stays running |
| `ADRIAN.old.exe` | beside the exe | The replaced copy until the new one is proven |
| `Local\ADRIAN-desktop-show` | named event, per user session | Single instance and "show the window" signal |

Apart from link.log (next release; 0.17.1 writes no log files), `eprintln!` output is
invisible in release builds. A dev build shares all of these with an installed copy, so a
dev build exits (and shows the installed window) while the installed app runs.

## Installer

`packaging/Install.ps1` (not covered by any automated test):

```
powershell -ExecutionPolicy Bypass -File crates\desktop\packaging\Install.ps1 -Server myserver:8791 -Token sat_xxxx
```

- Parameters: `-App` (default `ADRIAN.exe` next to the script, else
  `..\..\..\target\release\adrian-desktop.exe`), `-Server` and `-Token` (both or neither),
  `-NoStartup`, `-Uninstall`.
- With `-Server` and `-Token` it runs `--pair` first and stops on a non-zero exit before
  touching anything.
- Stops a running copy at the target path, copies to `%LOCALAPPDATA%\A.D.R.I.A.N\ADRIAN.exe`,
  creates `A.D.R.I.A.N..lnk` on the Desktop, in Start menu Programs, and in Startup with
  `--background` (unless `-NoStartup`, which removes it), then starts it (`--background` when
  `-Server` and `-Token` were given in this run, otherwise with the window so the settings
  are there).
- `-Uninstall` removes the shortcuts and the app folder and keeps the settings in `%APPDATA%\adrian`.
- First installs from a release use the zip asset; the updater uses the bare exe.

## Footprint (measured)

Measured on a desktop PC with a meter that counts the app and its child processes. Not
re-measured for this page.

| Date | Build | Result |
|---|---|---|
| 2026-10-22 | orb paused off screen | Hidden in the tray, the orb used to keep drawing: about 12 % CPU and GPU spikes to 19 % (about 12 % GPU, peaks 24 %, during a full-screen game). After the pause: 0 % CPU, 0 % GPU |
| 2026-10-23 | no window in the tray + quiet mode | Before: 7 processes, 303 MB private memory (296 MB of it WebView2), 0.04 % CPU. After: 1 process, 5 MB private, 0.01 % CPU (1.8 ms of CPU per second while speech was being forwarded), 0 % GPU, 0.3 KB disk I/O per minute |
| 2026-11-01 | 0.15.3 | Back on the server 1.2 s after an update swap; old copy removed about 3:10 after start; 4.8 MB private |
| 2026-11-01 to 2026-11-19 | releases 0.15.3 to 0.17.1 | 4.7 to 4.9 MB private memory after a restart |
| 2026-11-01 | `--self-check` on a release exe (just before 0.15.3) | 30 of 30 runs exited 0 in 15 to 18 ms (about 0.5 s when antivirus scans a new file) |
| 2026-11-20 | orb pacing, next release (headless Edge, not the app) | With the window open on a 120 Hz screen: 61 instead of 102 frames a second, Edge CPU 608 instead of 990 ms per second. See [Window, tray and orb](#window-tray-and-orb). Tray footprint unchanged by design (no window there) and not re-measured |

Network use follows from the format: 48 kHz x 16-bit mono = 96 KB/s (768 kbit/s) up while
the gate is open and the same down while audio plays; nothing goes up while nobody talks;
one small JSON ping per 20 s. Updates: one GitHub API call 3 minutes after start and every
6 hours, plus the downloads when there is a new version.

Money: the PC app calls no paid service. What it triggers on the server (speech segments,
typed text and voice requests to the model, `pc_screen` pictures as image input, voice
characters) is paid there; see the bot's
[costs.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/costs.md). False
gate openings (noise, shows) cost server CPU but are usually dropped before the model.

## Invariants and the tests that guard them

`cargo test -p adrian-desktop`: 116 test attributes (alarm 25, audio 15, config 11, link 38,
linklog 2, main 4, single 2, tray 4, update 15), 3 of them ignored by default. Reported runs
on the branch, 2026-11-20: 113 passed, 3 ignored (73 at 0.17.1, then 83, 92 and 106 over the
passes). `link.rs` tests start real WebSocket servers on `127.0.0.1:0` with shortened
timings under `cfg(test)` (`HELD` 500 ms, `PING` 300 ms, `SEND_LIMIT` 200 ms, `REFUSED_RETRY` 1.5 s; the reconnect
schedule constants are the production values). The ignored ones are measurements, not
checks: `reconnect_after_outages` (the outage harness; `ADRIAN_OUTAGES`, `ADRIAN_OUTAGE_RUNS`),
`reconnect_schedule_sweep` (the model above) and `default_output_form_reads` (reads the real
default output's form factor, read-only). `a_pc_held_up_past_the_servers_keep_alive_is_back_soon_after_it_wakes`
asserts a 700 ms bound in real time and can miss it on a heavily loaded machine.

| Invariant | Why | Guarded by |
|---|---|---|
| Nothing typed, pressed or recorded offline is sent later; orders from before a welcome are dropped; the mic queue is drained on connect | A stale "shut down" must not run later | `input_is_turned_down_offline_and_nothing_waits`, `orders_left_from_before_the_connection_are_never_sent`, `an_order_before_the_welcome_is_never_sent`, `audio_from_while_offline_is_never_sent`, `input_is_only_taken_while_the_link_is_up` |
| No PC action before the server accepted the token | Anything at the address could send `action` | `link.rs` action handling |
| The PC's policy decides, read per action | The local allowlist is the safety boundary | `Pc::run` in pc-control; the `Pc` swap in `save_settings` |
| Push-to-talk never outlasts the link; only a hotkey still held resumes | The mic must not open by itself after a reconnect | `the_hotkey_outlasts_the_link_and_the_button_does_not`, `a_hotkey_still_down_when_she_comes_back_counts_from_there`, `a_hotkey_let_go_before_she_comes_back_is_forgotten` |
| A press says it holds; the key-up is sent only while online | The server takes only what was said during the hold | `push_to_talk_says_it_holds_and_when_the_key_goes_up`, `the_talk_key_going_up_is_said_only_while_she_is_there` |
| The output kind is sent after every welcome and on change; a mark wins; unreadable is unknown | The server's call guard must never see headphones that aren't there | `headphones_are_what_windows_says_or_what_settings_mark`, `the_output_kind_is_said_after_the_welcome_and_again_when_it_changes`, `headphones_are_marked_by_the_output_in_use_at_save` |
| A notice chimes only after a welcome, short and quiet | Anything at the address could send one | `a_notice_plays_a_soft_chime_here_once_welcomed`, `the_notice_chime_is_short_and_quiet` |
| Her voice has no gaps from the queue lock; a device switch keeps it; after an outage over 1.5 s only fresh audio plays, except an alarm's | A late answer must not play long after it was said | `playing_never_waits_for_the_speakers_queue`, `the_speakers_wait_out_a_short_push_instead_of_playing_silence`, `a_device_switch_keeps_what_she_was_saying`, `speakers_back_after_an_outage_play_only_what_just_came`, `a_short_dropout_keeps_what_she_was_saying`, `an_alarm_ringing_keeps_its_sound_through_an_outage`, `what_came_during_an_outage_survives_a_rate_change_in_step` |
| link.log stays small and holds no server name or token | It is the file people paste when asking for help | `an_outage_is_logged_once_with_its_tries_and_nothing_secret`, `stays_small_and_writes_nothing_without_a_file`, `the_server_address_is_read_for_hiding` |
| Mic queue bounded at 30 s and never blocks | An unpaired PC must not grow memory | `audio_nobody_reads_is_capped_and_preroll_survives_it` |
| The gate doesn't go deaf after loud audio; her echo doesn't open it | Earlier versions learned her voice as the noise floor | `her_voice_and_loud_moments_dont_deafen_it`, `talking_over_her_cuts_in_but_her_echo_does_not`, `gate_opens_on_speech_with_preroll_and_closes_after_quiet` |
| Save is all or nothing | A refused token or bad shortcut must not half-apply | `a_refused_token_changes_nothing`, `a_failed_write_changes_nothing`, `an_unreadable_shortcut_is_named_and_stops_the_save` |
| The token stays encrypted at rest | The settings file is readable by other programs | `keeps_the_token_encrypted` (Windows) |
| Updates only from this project's release URLs, matching the checksum, passing `--self-check` with the exact tag, re-hashed before the swap, never while busy or behind a full-screen app, old copy kept until proven | A bad build must not brick PCs; an update mid-match would steal focus | `only_the_right_version_passes_the_self_check`, `a_download_that_wont_start_is_thrown_away`, `a_power_cut_at_any_step_of_a_swap_leaves_an_app_to_start`, `swaps_out_a_copy_that_is_running`, `a_new_copy_that_wont_start_is_swapped_back`, `the_old_copy_outlasts_the_start_of_the_new_one`, `the_old_copy_waits_for_a_welcome_and_the_full_wait` |
| Quick tries through a restart, slow through a long outage; backoff isn't reset by a connection that dropped within 30 s; refusals retry slowly | A restart should be short for the user; two PCs on one token; the server's lockout after 5 bad tokens | `tries_quickly_through_a_restart_and_slowly_through_a_long_outage`, `a_server_that_answers_and_doesnt_keep_it_is_backed_off_1_2_5_then_10`, `a_connection_dropped_right_after_its_welcome_keeps_backing_off`, `after_a_connection_that_held_the_next_try_comes_within_a_second` (under 900 ms), `a_refusal_is_tried_again_without_new_settings_but_slowly`, `a_lockout_close_counts_as_a_refusal`, `no_hello_is_a_slow_server_not_a_refusal` |
| A hung server or stuck send is a lost link; a PC sleep is not; a PC frozen past the server's keep-alive is back soon after it wakes | A hung process leaves the socket open | `a_server_that_stops_answering_is_a_lost_link`, `a_send_stuck_on_a_full_socket_is_a_lost_link`, `a_stall_counts_as_one_unanswered_ping`, `answered_pings_keep_a_quiet_link_up`, `the_servers_own_keep_alive_counts_as_an_answer`, `a_pc_held_up_past_the_servers_keep_alive_is_back_soon_after_it_wakes` |
| Alarms: the server wins when it speaks, but not with a list made before it heard an answer given here; a local ring survives a resync; each occurrence rings locally at most once; answers reach the server at once when linked, after the welcome when not, and stay pending if a send fails; no early ring after a clock jump outlives the next list; a ring isn't restarted after sleep; speakers raised only if needed and restored once, even after a crash | An alarm must ring when the server is down, but never twice, early or forever | 25 tests in `alarm.rs` and 3 in `link.rs`, e.g. `online_but_never_rung_by_the_server_it_rings_here`, `snoozed_here_while_linked_a_stale_resync_doesnt_lose_the_snooze`, `a_snooze_survives_a_restart_before_the_server_hears_it`, `rung_early_because_this_pcs_clock_jumped_ahead_it_waits_for_its_real_time`, `waking_from_sleep_long_after_the_server_rang_it_doesnt_start_it_over`, `an_alarm_answered_here_reaches_the_server_on_the_same_link`, `an_answer_that_fails_to_go_out_stays_pending` |
| No window in the tray; orb paused off screen; audio threads exempt from EcoQoS | Footprint, and audio that keeps working behind games | `a_paused_orb_still_hears_every_change_and_nothing_else`, `a_drawing_orb_gets_every_frame`, `pausing_again_starts_from_a_fresh_frame` |
| Never a focused window over a full-screen app | Games and anti-cheat don't tolerate focus theft | `show_for_alarm`, the updater's idle check |
| Tray tooltip at most 127 UTF-16 units | tray-icon writes past Windows' 128-unit field otherwise | `a_long_status_stays_inside_the_tooltip_field` |
| MSRV 1.88 | Workspace `rust-version` | Tauri pins; `cargo +1.88.0 check --workspace --all-targets` |
| Static C runtime | The exe runs without the VC++ Redistributable | `.cargo/config.toml` |

## Known issues (open)

Found by a review of the 2026-11-20 passes (with probe tests that show each behavior). The
PC-side items were checked again in the code on this branch; R2 and the server half of A2
rest on the review's reading of the bot. **Not fixed yet**; a later pass is planned. Letters
are the review's.

| | Issue | Effect | Since |
|---|---|---|---|
| A1 | An alarm answer written to a link that has died without a reset (Wi-Fi drop, NAT timeout) counts as sent once the write succeeds, and is forgotten. The PC notices the dead link only 40-60 s later | After the reconnect the server is still ringing it: the alarm rings again and the Stop or Snooze is lost. 0.17.1 held the answer for the next welcome, so this is new with the "send at once" change. Fix: forget an answer only once the server's list shows the occurrence moved on | next release (introduced) |
| A2 | The clock-jump check runs only when a new list arrives (hourly or on change). After the PC's clock jumps ahead (a manual change, or a dual-boot machine whose other OS keeps the hardware clock in UTC), an alarm can ring early for its whole ring time | A Stop during that early ring goes to the server, which finishes the real occurrence before it was due: that alarm never rings, with no sign | older; made likelier by sending answers at once |
| A3 | An answer marker is dropped when the server lists that occurrence under `ringing` (ringing alarms are not in `list`) | After a mid-ring link drop and a Stop here, the window shows it ringing again until the welcome's `alarm_done` lands; this is what makes A1 lose the answer | next release |
| A4 | Every failed reconnect try clears the playback queue (`stop_reply`), including the PC's own alarm beeps | The backup ring has gaps of up to about 1 s on each try, roughly 15 % of the ring missing during the first 30 s of quick tries | older; more tries now |
| A6 | The local ring length is measured on the wall clock | A clock set back an hour mid-ring stretches the ring to about 70 minutes unless answered (Stop still works) | older |
| S1 | The stale-speech clock starts when the failure is noticed, not when audio last played | After sleep (or a stream that never reports an error), an answer queued before the sleep can still play on waking. Bounded by what was queued, normally under a second with the server's real-time pacing | next release |
| S2 | A device that opens and then fails at once (a flaky USB cable) restarts the outage clock and the 300 ms retry on every open | Its queue is never aged and the retry never backs off to 5 s | next release |
| S3 | `open_streams` needs a working default mic before it opens the speakers | With the mic unplugged or turned off, nothing plays at all, **the backup alarm ring included** | older |
| R2 | The bot counts "no hello" toward its address lockout | Repeated multi-second stalls of the server (not the PC) between the upgrade and reading the hello could lock the PC out for about 10 minutes, plus up to 5 minutes of refusal wait. The fix is on the bot side | older |
| L1 | Only the server's host name and the token are blanked in link.log | A TLS certificate-name error from rustls lists every DNS name and IP address on the server's certificate, which can include private names or LAN addresses. Local only, but check the file before sharing it | next release |
| L2 | link.log's 128 KB bound is checked before a write, and lines aren't capped | One server-sent refusal reason can be very large, so one line can make the file big until the next line rotates it; a viewer holding the file open without share-delete stops rotation | next release |

Also still open:

- **Links dropping when a game starts** and coming back after more than a minute are not
  explained. link.log is there to settle it at the next occurrence; `PING`, `UNANSWERED`,
  the connect limit and quiet mode were deliberately left unchanged.
- **Not tried live:** reconnect against a real server restart, a device switch while she
  talks, the headphones switch in a running window, a live alarm ring on the new code. All
  new behavior is unit-tested only.
- The orb was measured in headless Edge, not in the app's WebView2 window.

## Known limitations

- Windows only in practice. Other platforms compile against stubs (no DPAPI, so no token can
  be saved; no game or full-screen detection; no quiet mode).
- No acoustic echo cancellation; barge-in relies on the learned coupling and a 3x margin.
  `examples/aec_probe.rs` tried Windows' communications-stream echo cancellation; not adopted.
- On 0.17.1 and older, `ptt` is sent only on press and the speaker callback plays a frame of
  silence when the queue lock is busy; both are fixed in the next release.
- The call guard depends on what the PC says about its output. Calls the bot can't see
  (servers it isn't in, direct-message calls, other apps, a video playing) still reach a
  PC on speakers hands-free (roadmap F1).
- Unused data: `owner` from `welcome`, the alarm `sound` field.
- The release exe is not code-signed (SmartScreen warns on first run).
- Anyone talking near a paired PC speaks with its owner's rights (roadmap F1); the per-PC
  policy and the spoken yes for power actions are the stopgap. See [SECURITY.md](SECURITY.md).
- Dev builds self-update too: `update::run` isn't gated by build type, so a local build older
  than the newest release replaces its own exe about 3 minutes after start unless auto-update
  is off.
- A corrupt `config.json` silently loads as defaults and the next Save overwrites it.
- Status text and messages are English-only strings in code.
- Stale comments to be aware of: the `audio.rs` module doc says the gate stays shut while she
  talks (the code is full duplex); the `orb.js` header mentions a mouse lean that `app.js`
  never uses and doesn't list `setActive`; the `STALE` comment in `audio.rs` names sleep as
  covered (see S1). The comment near the device-change handling that says queued audio
  carries on after a switch is now true.
- Code comments and doc strings call the next release "0.17.2"; the version isn't decided.

## Change guide

| Change | Where | Care |
|---|---|---|
| New message either way | `link.rs` `session` match (server to PC) and `Cmd` (PC to server); bot `src/satellite/connection.ts` and `docs/satellite.md` | Additive only. Unknown types are ignored by this client; the server learns the app version from `hello.version` and must keep working with older apps. Add a `link.rs` test with the local WebSocket helpers (`serve`, `orders`, `heard_until`) |
| Gate or barge-in tuning | constants at the top of `audio.rs`, `Gate::frame`, the coupling code in `audio::start` | Every change moves false triggers and missed words. Keep the gate tests, add a case, and check by ear on real hardware (it plays sound) |
| New PC action or permission | `crates/pc-control` (tool, `allowed`, `Policy` field), the toggle in `PERMS` in `ui/app.js`, the tool spec in the bot | See [pc-control.md](pc-control.md#change-guide) |
| New setting | `Settings` (`#[serde(default)]`), `Form`, `commit`, `SettingsView`, `save_settings` args (Tauri passes camelCase from JS), HTML/JS | Keep `commit` all or nothing; add a config test |
| Window or UI | `ui/*` (rebuild to see it) | CSP forbids inline scripts and remote content. New window APIs need a permission in `capabilities/default.json`; new commands go in `generate_handler!` |
| Orb look or pacing | `ui/orb.js` | Needs WebView2 `ctx.filter`. Keep `setActive`, or the hidden-window GPU cost comes back. Keep motion timed by the clock (`ease`, `TUNED_HZ`), not per frame. Measure before and after with `bench/orb-bench.mjs` (timing A/B against the old file, and `--compare` for pixels) |
| Reconnect schedule | `Retry`, `retry_wait`, `short_wait` in `link.rs` | Run the ignored `reconnect_schedule_sweep` and `reconnect_after_outages` before and after. Keep `short_wait` for servers that answer and don't keep the link |
| link.log lines | `Trace::ended`, `Away`, `here` in `link.rs`; `linklog.rs` | Never write words, transcripts, device or user names. Everything goes through `LinkLog::line`, which blanks the host and token |
| Updater | `update.rs` | Keep the asset names, the URL prefix check, the checksum, the `--self-check` contract and the old-copy rule |
| Alarms | `alarm.rs` (pure logic, well tested), alarm thread in `main.rs` | A live ring makes sound and opens a window |
| Version bump | `Cargo.toml` and `tauri.conf.json` together, to the bot's release number | Write `tauri.conf.json` without a BOM |
| Dependencies | `Cargo.toml` | Tauri must keep MSRV 1.88 (or raise the workspace MSRV on purpose). `windows` 0.58 across the workspace; `PROPERTYKEY` is in `UI::Shell::PropertiesSystem` in that version |

Safe to change: UI strings and layout, tray text, the orb's look, test helpers, quiet-mode
thresholds (measure before and after). Risky: link gating and backoff, gate math, the
updater, the DPAPI storage format (changing `token_protected` unpairs every PC), the settings
location (moves everyone's settings), the single-instance event name (two copies could run),
asset naming.

Commands:

```
cargo build --release -p adrian-desktop   # target\release\adrian-desktop.exe
cargo test -p adrian-desktop
cargo clippy -p adrian-desktop
cargo +1.88.0 check --workspace --all-targets
cargo run --release -p adrian-desktop --example aec_probe -- <voice.wav> <out dir>   # plays sound

# measurements (ignored by default)
cargo test -p adrian-desktop --bin adrian-desktop reconnect_schedule_sweep -- --ignored --nocapture
cargo test -p adrian-desktop --bin adrian-desktop reconnect_after_outages -- --ignored --nocapture --test-threads 1
cargo test -p adrian-desktop default_output_form_reads -- --ignored --nocapture   # read-only

# orb bench, from crates/desktop/bench (Node 22+, Microsoft Edge; headless, no window)
git show <old commit>:crates/desktop/ui/orb.js > old-orb.js
node orb-bench.mjs --ref old-orb.js --base empty-orb.js --hz 120 --secs 15 --runs 5 --out ab-120.json
node orb-bench.mjs --compare --ref old-orb.js --out cmp.json
node orb-bench.mjs --secs 8 --profile --out prof.json
```

The bench starts headless Edge processes with temporary profiles; it stops and removes them
after each run. Run it at low priority on a machine someone is using.

For release builds, set `CARGO_HOME` to a folder under `target\` so `aws-lc-sys` doesn't embed
the builder's home path in the exe. In some terminals, processes started from a tab are killed
when the tab closes (Windows job objects); start a test copy detached. Running `--pair` on a
machine replaces that machine's real pairing.
