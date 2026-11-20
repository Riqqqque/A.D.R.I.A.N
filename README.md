# A.D.R.I.A.N.

**A**dvanced **D**igital **R**easoning & **I**ntelligence **A**ssistant **N**etwork

A.D.R.I.A.N. is a self-hosted voice assistant suite. You talk to it by saying "Adrian" in a Discord voice call or at your Windows PC. It answers out loud, plays music, rings alarms, remembers people and controls the PC. It is built to run on your own hardware, keep data private by default, and cap what it spends.

This repository is the Rust half of the suite:

- **The Windows PC app** (`crates/desktop`): a tray app with an optional animated window. It listens through the PC's microphone, plays her voice through the speakers and carries out PC actions. It is released and installed today.
- **PC control** (`crates/pc-control`): the library that carries out those actions (apps, windows, web pages, media, volume, power, status, clipboard, screen), plus the `pcctl` command-line tool.
- **An older Rust household server** (`crates/server`, `crates/satellite`, `crates/protocol`, `web/`): web UI, chat with typed tools, Home Assistant lights, memory, alarms and budgets. It is maintained and tested, but it is **not what runs in production** and nothing else in the suite talks to it. See [The older Rust household server](#the-older-rust-household-server).

What runs in production is the TypeScript bot in the other repository, **[adrian-discord-voice](https://github.com/Riqqqque/adrian-discord-voice)**. It is the Discord bot and the server the PC app connects to. Everything that costs money (speech recognition, the language model, the cloud voice, web search) happens there.

**Changing the code?** Start with [CONTRIBUTING.md](CONTRIBUTING.md). It covers the reading order, what is safe to change, what is guarded and by which tests, and how to test on a real PC without harming it.

---

## The suite at a glance

| Module | Where | Language | Status (2026-11-20) |
|---|---|---|---|
| Discord voice bot and PC server ("satellite" server) | [adrian-discord-voice](https://github.com/Riqqqque/adrian-discord-voice) | TypeScript, Node.js 22, Linux | Production. Release 0.17.1. |
| PC app | `crates/desktop` (this repo) | Rust, Tauri 2 (WebView2), Windows | Released with every bot release as `adrian-pc-app-<version>-windows-x64.exe` / `.zip`. Version 0.17.1. Updates itself. |
| PC control and `pcctl` | `crates/pc-control` (this repo) | Rust, Win32/WinRT | Used only by the PC app. Version 0.1.0. |
| Household server, old satellite, protocol, web UI | `crates/server`, `crates/satellite`, `crates/protocol`, `web/` (this repo) | Rust (axum, SQLite), SvelteKit | Preview ("Gate A + early Gate B", defined [below](#the-older-rust-household-server)). Not in production. Not connected to the bot or the PC app. |

The two repositories stay separate on purpose. The PC app's updater downloads new versions from the bot repository's GitHub releases (`REPO` in `crates/desktop/src/update.rs`), so every bot release also carries the Windows exe. The PC app's version number tracks the bot's release number. `crates/pc-control` and the server crates stay at `0.1.0`.

## How the live pieces connect

```
Windows PC: crates/desktop (ADRIAN.exe)            Bot server: adrian-discord-voice
                                                    (satellite.enabled, port 8791)

hello {token}  ---------------------------------->  checks the sat_ token
               <----------------------------------  welcome
mic -> loudness gate (300 ms pre-roll)
speech_start, 48 kHz mono PCM, speech_end  ------>  Silero VAD -> Parakeet ASR
                                                    -> wake check ("Adrian")
                                                    -> conversation controller
                                                    -> language model + tools
speakers <- playback queue  <---------------------  her voice, music, sounds (48 kHz PCM)
orb window, tray, transcript  <-------------------  state, mode, transcript, alarms
crates/pc-control: policy check,  <---------------  action {name: pc_open, pc_volume, ...}
  then the Win32/WinRT call  -------------------->  action_result {ok, result, image?}

self-update (every 6 h)  <------------------------  GitHub releases of the bot repo
```

The PC shares one brain, memory, set of people, limits and spend caps with the Discord call.

- The PC only does the cheap local part: a loudness gate decides when someone may be talking, and only then streams audio up. Exact speech detection, the wake word, speech recognition, the model, the tools, the voice, music and alarms all run on the server.
- The model on the server has nine PC tools: `pc_apps`, `pc_windows`, `pc_open`, `pc_media`, `pc_volume`, `pc_system`, `pc_info`, `pc_clipboard`, `pc_screen`. Each PC decides in its own settings which of them it allows, and the server cannot change that. Sleep, restart and shut down also need the person's own spoken yes.
- The wire protocol is in the bot repo: [docs/satellite.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md). This repo's `crates/protocol` is a different protocol, used only by the old Rust satellite and server.

More detail: [docs/architecture.md](docs/architecture.md) (crates and how they connect), [docs/desktop.md](docs/desktop.md) (PC app internals), [docs/pc-control.md](docs/pc-control.md) (every PC action and its safety rules).

## What the PC app does

- **Always listening, cheaply.** Say "Hey Adrian", or hold **Ctrl+Alt+Space** (push-to-talk), or type in the window. Follow-ups without her name work for a short window after she answers; the server decides how long. From the next release push-to-talk lasts exactly as long as you hold the key: what you say while holding it is hers, nothing after.
- **Knows when the mic hears your call** (next release). The app tells the server whether her sound plays on headphones or speakers. While you're in a Discord voice call the bot can see and the PC is on speakers, she takes only push-to-talk and typing there, and her name said aloud gets a soft chime instead of an answer. Many USB DACs and docks report "speakers" even with earphones plugged in: tick **These speakers are headphones** in settings once for that output.
- **Talk over her.** The mic stays open while she speaks. The app learns how much of her voice leaks back into the mic. It lowers her to 30% on the PC the moment you talk over her, and the server then stops her. Music alone is never ducked for noise.
- **PC actions**, typed and allow-listed (`crates/pc-control`):
  - open, close, focus or minimize apps and games, and list what's open;
  - snap, move, maximize, center or restore windows across monitors, and "put it back";
  - open public web pages, YouTube, Spotify searches and known folders;
  - play, pause, skip or check what's playing (Windows media sessions, not key presses);
  - master, per-app or "only what's playing" volume;
  - lock, screens off, and sleep, restart or shut down only after a spoken yes (restart and shut down wait 30 s and can be cancelled);
  - PC status and a health check (CPU, memory, disks, NVIDIA GPU);
  - the clipboard text and a picture of a window or screen, only when asked;
  - the PC's own named shortcuts from its settings (an `.exe`, `.lnk`, `.bat` or `.ps1` file, or a URL). Only these and installed apps can be started.
- **Leaves running games alone.** It doesn't type or click into other programs or kill processes, and windows of games and of programs it can't identify are never closed, moved or pictured. Two exceptions are still open (see [docs/ROADMAP.md](docs/ROADMAP.md#safety-and-rights)): "screens off" broadcasts a system message to every window, and "show desktop" minimizes everything.
- **Quiet while gaming.** Every 2 s it checks whether a game is in front (after 5 s in a game, ended after 20 s out) and tells the server, which then answers only to her name, keeps answers short and holds reminders until the match ends.
- **Alarms at the PC.** It keeps the server's upcoming alarms. If the server hasn't rung one 20 s after it's due, or the link drops mid-ring, it rings the alarm itself with a beep pattern. It has Stop and Snooze buttons in the window and tray, and raises speakers that are below 30% or muted for the ring, then puts them back. From the next release an answer given at the PC reaches the server at once when linked and survives a stale list from the server; known gaps are in [docs/desktop.md](docs/desktop.md#known-issues-open).
- **Stays out of the way.** In the tray there is no window at all; the web view exists only while the window is open. The process drops to low priority and EcoQoS when idle. The orb stops drawing when it's off screen or a full-screen app is in front, and from the next release it draws at most about 60 times a second on any monitor (about 40% less CPU with the window open on a 120 Hz screen, measured in headless Edge).
- **Updates itself** from GitHub releases, checked 3 minutes after start and then every 6 hours. Each download is checked against its `.sha256` and must pass `--self-check` before it's used. It installs only while she's idle with no window and no full-screen app in front, and keeps the old copy until the new one has connected and lasted 3 minutes.
- **Follows the Windows default devices** when you switch speakers or microphone; from the next release what she's saying carries on on the new device.
- **Comes back quickly** (next release): about a second after a server restart, and a small local `link.log` records why a link dropped (server name and token blanked).

Setup (turning the PC server on in the bot, pairing, installing) is in the bot repo's [docs/pc-app.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/pc-app.md). Internals are in [docs/desktop.md](docs/desktop.md).

## What it costs

**On the PC: nothing paid.** Neither `crates/desktop` nor `crates/pc-control` calls a paid service. The PC app only talks to your own bot server and to GitHub for updates.

| Cost | Amount | Source |
|---|---|---|
| Upload while the gate is open | 48 kHz × 16-bit mono = 96 KB/s (768 kbit/s); nothing while nobody talks | `crates/desktop/src/audio.rs` |
| Download while she plays audio | 96 KB/s | same format |
| Keep-alive | one small JSON ping every 20 s | `crates/desktop/src/link.rs` |
| Update checks | one unauthenticated GitHub API call 3 min after start, then every 6 h | `crates/desktop/src/update.rs` |
| Memory in the tray | about 5 MB private, 1 process (0.15.3 to 0.17.1 measured 4.7 to 4.9 MB after a restart) | measured on a Windows desktop, 2026-10-23 to 2026-11-19 |
| CPU and GPU in the tray | about 0.01% CPU, 0% GPU, about 0.3 KB disk I/O per minute | measured 2026-10-23 |
| Window open | the WebView2 window adds about 300 MB (measured 296 MB) and a few processes; it is destroyed when closed | measured 2026-10-23, comment in `main.rs` |

Before the tray redesign on 2026-10-23, the app used 7 processes and 303 MB of private memory. Before the orb paused off screen on 2026-10-22, it cost about 12% CPU, and GPU spikes up to 19%, while hidden.

**On the server: everything else.** What the PC does turns into server cost: each gate opening is a speech segment the server must check (local and free, but CPU). Requests for her and typed text become language model calls. Screen pictures (JPEG, long side at most 1568 px, aimed under 1 MB) go to the model as image input. Replies may use cloud voice characters. Numbers, caps and how to spend less: the bot's [docs/costs.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/costs.md).

**The household server** ships with no models and no prices. You enter prices yourself, local endpoints count as free, and monthly budgets cap paid calls (see [below](#usage-and-spending-caps)). It has never been measured against a real provider.

## Repository layout

| Path | Package | What it is | Test attributes (2026-11-20) |
|---|---|---|---|
| `crates/desktop` | `adrian-desktop` 0.17.1 | The PC app. Tauri `=2.11.6`, tauri-build `=2.6.3`. UI in `crates/desktop/ui` (embedded in the exe). Installer `crates/desktop/packaging/Install.ps1`. Orb bench in `crates/desktop/bench`. | 116 (3 ignored measurements; 73 at 0.17.1) |
| `crates/pc-control` | `adrian-pc-control` 0.1.0 | PC actions library and the `pcctl` CLI (`src/bin/pcctl.rs`). No network code apart from the DNS lookup `pc_open` makes to check a page's addresses; the library writes no files (`pcctl --out` saves a screenshot when asked); no paid service. | 109 (1 ignored live test) |
| `crates/server` | `adrian-server` 0.1.0 | Older household server: axum HTTP API, one SQLite worker thread, providers, Home Assistant lights, scheduler, usage. | 157 |
| `crates/satellite` | `adrian-satellite` 0.1.0 | Older Windows voice client for `crates/server` (push-to-talk, Windows SAPI voice, optional rustpotter wake word). Superseded by `crates/desktop`; whether to keep it is undecided. | 5 (1 ignored live TLS test) |
| `crates/protocol` | `adrian-protocol` 0.1.0 | JSON wire types between `crates/server` and `crates/satellite` only. | 4 |
| `web/` | `adrian-web` 0.1.0 | SvelteKit single-page UI for `crates/server`, built statically and served by it. | 10 (`npm test`) |

Test numbers are counts of `#[test]` / `#[tokio::test]` attributes in the source, not a record of a run ([TEST_REPORT.md](TEST_REPORT.md) has the reported runs). CI (`.github/workflows/ci.yml`, added 2026-11-20) runs clippy and the tests on Windows for the whole workspace, on Linux for the protocol and server crates, and the MSRV check; see [CONTRIBUTING.md](CONTRIBUTING.md#continuous-integration).

Workspace facts: edition 2024, `rust-version = "1.88"` (MSRV), release profile `lto = "thin"`, `codegen-units = 1`. `.cargo/config.toml` links the C runtime statically for `x86_64-pc-windows-msvc`, so the exe runs without the Visual C++ Redistributable.

## Quick start: the PC app

You need a bot server with the PC server turned on, and a `sat_` pairing token from it (`sudo adrian satellite add "Desk PC" <owner's Discord user ID>` on the server; see [docs/pc-app.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/pc-app.md)).

Download `adrian-pc-app-<version>-windows-x64.zip` from the [bot's releases](https://github.com/Riqqqque/adrian-discord-voice/releases) and check it against its `.sha256`, or build it yourself with Rust 1.88 or newer and Visual Studio Build Tools:

```powershell
cargo build --release -p adrian-desktop
# exe: target\release\adrian-desktop.exe

powershell -ExecutionPolicy Bypass -File crates\desktop\packaging\Install.ps1 -Server myserver:8791 -Token sat_xxxx
```

`Install.ps1` pairs first (and stops if that fails). It copies the app to `%LOCALAPPDATA%\A.D.R.I.A.N\ADRIAN.exe`, adds Desktop, Start menu and Startup (`--background`) shortcuts, and starts the app. `-NoStartup` skips the Startup shortcut and `-Uninstall` removes the app and shortcuts but keeps the settings. Settings live in `%APPDATA%\adrian\desktop\config\config.json`; the token is stored encrypted with Windows DPAPI for the current user.

Command-line flags: `--pair SERVER TOKEN`, `--background`, `--update-now`, `--self-check`, `--after-update` (used by the updater). Details: [docs/desktop.md](docs/desktop.md).

Read-only checks with `pcctl` (safe on a PC someone is using). The quoting below is for a POSIX shell such as Git Bash; PowerShell needs the JSON quoted its own way.

```sh
cargo run -p adrian-pc-control --bin pcctl -- pc_info '{"action":"health"}'
cargo run -p adrian-pc-control --bin pcctl -- pc_windows '{"action":"monitors"}'
cargo run -p adrian-pc-control --bin pcctl -- pc_volume '{"action":"get","playing":true}'
cargo run -p adrian-pc-control --bin pcctl -- pc_apps '{"action":"which","name":"spotify"}'   # dry run: what "open" would start
```

Output is `ok|no (<ms> ms) <text>`; exit code 0 = done, 1 = refused or failed, 2 = usage or bad JSON.

## Build and verify

```sh
cargo fmt --all --check
cargo clippy --workspace
cargo test --workspace                       # builds and tests every crate, the server too
cargo test -p adrian-desktop
cargo test -p adrian-pc-control
cargo +1.88.0 check --workspace --all-targets   # MSRV check
cd web && npm ci && npm run check && npm run build && npm test
```

Before running tests or a dev build on a PC that also has the app installed, read the live-testing section in [CONTRIBUTING.md](CONTRIBUTING.md#testing-on-a-real-pc). A dev build shares the installed app's settings and can update itself.

---

## The older Rust household server

`crates/server` with `web/`, `crates/satellite` and `crates/protocol` is the first module of the suite: a household assistant with an authenticated local web UI. It was built first (2026-09-17 onward). Its latest server changes are from 2026-11-01. It is tested code, but the suite moved on: the Discord bot was built as an independent TypeScript service with its own memory, alarms, budgets and Home Assistant access, and the PC app pairs with the bot, not with this server. Nothing in the bot or the PC app calls this server.

Status: **Gate A preview + early Gate B.** Gate A is the server core: setup claim, accounts and sessions, memory, alarms, diagnostics, backup, and strict-local egress checks. Gate B is the old voice satellite: pairing, device tokens, spoken utterances, WebSocket sessions and alarms rung on a satellite. Both are the row groups of [TEST_REPORT.md](TEST_REPORT.md) (`A-*` and `B-*`). It has never been run against a real AI provider or a real Home Assistant, and the tray, wake word and SAPI output have not been exercised on hardware (see the "not run" rows in [TEST_REPORT.md](TEST_REPORT.md)). Before putting work into it, check with the maintainer whether the work belongs here or in the bot.

### What it has

- **Setup and accounts.** On first boot a one-use setup token is printed and saved to `SETUP_TOKEN.txt` in the data directory; it creates the owner. 7-day session cookies, Argon2id passwords, CSRF header on writes. Roles `owner`, `member`, `guest` exist in code (guest may only chat), but no API creates members or guests yet, so it is single-owner in practice.
- **Chat with typed tools** (`POST /api/chat`, streamed). OpenAI Responses API or any OpenAI-compatible endpoint (OpenAI, DeepSeek, llama.cpp, LM Studio and the like). Nine tools: `get_current_time`, `list_lights`, `control_light`, `remember`, `recall`, `set_alarm`, `list_alarms`, `cancel_alarm`, `list_audio_endpoints`. At most 8 tool rounds of 8 calls and 120 s per turn; one turn per person at a time.
- **Fast path** with no model call: exactly "what time is it", or one whole light command for one allow-listed light ("turn off the kitchen light").
- **Home Assistant, lights only.** Import keeps only `light.*`. The owner allow-lists each light. A command is reported `confirmed` only when polling `/api/states/{id}` (every 400 ms for up to 5 s) shows the new state, otherwise `accepted_unconfirmed`. There is no generic service passthrough.
- **Memory.** Private or household facts, full-text search (FTS5), edit history, tombstones. Household scope only when the person's own message asks for it.
- **Alarms and timers.** Saved to SQLite before the reply. One delivery per occurrence. A restart or restore never re-rings. Delivered to a Home Assistant media player, a browser tab ("Use this browser as a speaker") or a paired satellite, as a generated tone.
- **Old Windows satellite.** 6-digit pairing codes (5 minutes, one use, rate-limited), `sat_` device tokens stored hashed and revocable. Voice over a WebSocket session (`GET /api/satellite/session`) or a one-shot WAV upload. Local alarm replicas ring on the satellite when the server can't deliver.
- **Usage and budgets.** Every model round and transcription is metered. Prices are entered by you. Monthly budgets per household, provider or module, soft warning at 80%.
- **Security.** Provider keys and the Home Assistant token are encrypted at rest (ChaCha20-Poly1305, key DPAPI-wrapped on Windows). Optional built-in TLS. Trusted-proxy handling. Strict-local privacy mode. No telemetry. See [docs/SECURITY.md](docs/SECURITY.md).

### Requirements

- Rust 1.88 or newer, MSVC toolchain on Windows, plus a C compiler (Visual Studio Build Tools) for bundled SQLite.
- Node.js 18 or newer for the web UI build.
- Optional: a Home Assistant instance with a long-lived access token; an OpenAI, DeepSeek or local OpenAI-compatible endpoint.

### Run it

```sh
# 1. Build the web UI (once, or after frontend changes)
cd web
npm ci
npm run build
cd ..

# 2. Build and start the server
cargo build --release -p adrian-server
set ADRIAN_DATA_DIR=%USERPROFILE%\.adrian      # or: export ADRIAN_DATA_DIR=~/.adrian
.\target\release\adrian-server.exe             # or ./target/release/adrian-server
```

Open `http://127.0.0.1:8787`, paste the setup token, and create the owner account. Then:

- **Integrations**: add an AI provider (keys stay on the server) and/or connect Home Assistant (URL and long-lived token), then *Import entities* (lights only).
- **Devices**: allow-list the lights it may control. Nothing is controllable until you tick it. Voice satellites and pairing codes are here too.
- **Audio**: *Use this browser as a speaker* (the tab must stay open), or add a `media_player.*` endpoint. An alarm can't be created until an audio endpoint exists.
- **Alarms**, **Memory**, **Chat**.
- **System**: diagnostics, action ledger, backup, usage and budgets. Restore is API only: `POST /api/diagnostics/restore` (owner; keeps a `.pre-restore` copy and rolls back on failure).

After `npm run build`, restart the server. It hashes the inline boot script into its Content-Security-Policy at start, and a stale hash leaves the UI blank.

### Configuration

| Env var | Default | Meaning |
|---|---|---|
| `ADRIAN_DATA_DIR` | `./data` | database, backups, setup token, default key file |
| `ADRIAN_BIND` | `127.0.0.1:8787` | bind address (`0.0.0.0:8787` for the LAN). An unparsable value silently falls back to the default. |
| `ADRIAN_WEB_DIR` | `web/build` | built frontend location |
| `ADRIAN_TLS_CERT` / `ADRIAN_TLS_KEY` | unset | PEM certificate chain and key; both set means HTTPS only, one alone is a startup error |
| `ADRIAN_TRUSTED_PROXY` | unset | IP address of the TLS reverse proxy in front; only it may set `X-Forwarded-For` and `X-Forwarded-Proto` |
| `ADRIAN_KEY_FILE` | `<data dir>/secret.key` | key that encrypts stored secrets (DPAPI-wrapped on Windows) |
| `RUST_LOG` | `info,adrian=debug` | log filter |

TLS is off by default. For LAN use, set `ADRIAN_TLS_CERT`/`ADRIAN_TLS_KEY` or put a TLS-terminating reverse proxy in front and set `ADRIAN_TRUSTED_PROXY` to its address. The proxy must set `X-Forwarded-Proto` itself and append the client to `X-Forwarded-For`. A self-signed test certificate must be a leaf certificate, or rustls rejects it with `CaUsedAsEndEntity`:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem -days 365 -subj "/CN=adrian" -addext "subjectAltName=DNS:adrian.example.com,IP:192.0.2.10" -addext "basicConstraints=critical,CA:FALSE" -addext "extendedKeyUsage=serverAuth"
```

Only stored secrets are encrypted. Memory, chat history, sessions and backups are not; diagnostics says so. `secret.key` is not in backups: restoring on another machine means entering provider keys and the Home Assistant token again.

### Usage and spending caps

Every chat round and speech-to-text call is metered with the token counts the provider reports (the OpenAI-compatible adapter asks for them with `stream_options.include_usage`). When a provider reports nothing, an estimate is recorded and marked as one. Costs come from prices you enter under **System → Usage and budgets**; no prices are built in, because they change. Local endpoints (loopback, private or link-local addresses, `localhost`, `*.local`, dotless host names) always count as free and are never blocked.

Budgets are monthly, by calendar month in the household time zone, and apply to the whole household, one provider, or one module (`web` or `satellite`; `discord` is accepted but nothing records under it). At the soft limit (80% by default) you get one warning per budget per month. At the limit paid calls are refused with the actual numbers; local models keep working. The limit is checked before every tool round, so a long tool loop can't run past it. While any budget covers it, a cloud model **without a price is refused**, because a budget can't bound what it can't price.

API, owner only: `GET /api/usage?month=YYYY-MM`, `GET/PUT/DELETE /api/usage/prices`, `GET/POST /api/usage/budgets`, `DELETE /api/usage/budgets/{id}`.

### The old voice satellite

```sh
cargo build --release -p adrian-satellite

# In the web UI: Devices → Voice satellites → Generate pairing code
.\target\release\adrian-satellite.exe pair --server http://127.0.0.1:8787 --code 123456 --name "Desk PC"
.\target\release\adrian-satellite.exe run      # hold Ctrl+Alt+Space, speak, release
.\target\release\adrian-satellite.exe status   # verify the device token
.\target\release\adrian-satellite.exe say "test"  # verify speech output
```

`pair` overwrites the satellite config in `%APPDATA%\adrian\satellite\config\config.json`, so don't run it on a machine whose pairing you want to keep.

How it works: the satellite keeps a WebSocket session with a bearer device token. While the hotkey is held, or after a wake-word hit, it streams PCM between `AudioSessionStart` and `AudioSessionEnd` (`crates/protocol`, versioned JSON with a catch-all `Unknown` message type). The server transcribes through the default provider's `/audio/transcriptions` endpoint, runs the normal chat turn as the user who paired the satellite, and sends `Speak` back. The satellite speaks it with Windows SAPI. Speech-to-text uses the same provider as chat; for `openai_compatible` providers it sends the chat model's name. A chat-only cloud provider therefore can't transcribe, while local whisper servers that ignore the model name work. Strict-local mode blocks cloud speech-to-text.

A tray icon shows connection and mic state and has a Quit item. Losing the mic (unplugged) re-opens the default device instead of ending the client.

Alarms: a satellite can be an audio endpoint. The scheduler pushes `Speak` and waits for the playback ack; an offline satellite records a `failed` delivery, never a silent miss. The server also pushes the next occurrence of each alarm routed to the satellite (`AlarmReplicaSync`, on connect and after every change), and the satellite keeps them in `replicas.json`. The satellite rings a replica itself (tone plus spoken label, three rounds) only when it can't reach the server at due time, or when the server hasn't delivered it 60 s after due. On reconnect it reports what it rang (`AlarmLocalReport`), and the delivery is recorded as rung locally (audibility not confirmed). Alarm `Speak` messages carry the alarm id and occurrence, so a satellite that already rang an occurrence acks a late server delivery without ringing twice. Replicas more than 6 h overdue are dropped without ringing.

Wake word (opt-in): `cargo build --release -p adrian-satellite --features wake-word` (pulls the candle ML stack), then set `wake_word.model_path` to a rustpotter `.rpw` model and an optional `threshold` in the satellite config. Spotting runs locally on a 2-second rolling buffer; capture ends after about 0.9 s of quiet. No model ships with the repo.

Self-signed server certificate: `adrian-satellite pair --server https://adrian.example.com:8787 --code 123456 --ca cert.pem` stores the path as `ca_cert` and trusts it in addition to the system roots.

### Design rules the server code enforces

- The model proposes typed actions; the server checks role, scope, entity, action and parameters before anything runs. No unrestricted Home Assistant passthrough, no model-invoked shell.
- Physical actions are reported as *accepted* or *confirmed*, never more than what was observed.
- Alarms are persisted before acknowledgment; deliveries are unique per `(alarm_id, occurrence)`; a restart reconciles honestly instead of re-ringing.
- All SQLite access runs on one bounded worker thread; WAL, `synchronous=FULL`.
- Text in the prompt's data block (facts, light names) can't add rules or close the block.
- No telemetry. Strict-local mode refuses non-local provider endpoints.

---

## Documentation

This repository:

| Doc | What's in it |
|---|---|
| [CONTRIBUTING.md](CONTRIBUTING.md) | Start here before changing anything: ground rules, safe vs guarded areas, invariants and the tests that guard them, live-testing hazards, releases. |
| [docs/architecture.md](docs/architecture.md) | The crates and how they connect, inside the repo and to the bot. |
| [docs/desktop.md](docs/desktop.md) | PC app internals: link, mic gate, playback, alarms, updater, settings, installer. |
| [docs/pc-control.md](docs/pc-control.md) | Every PC action, its arguments, refusals and safety rules. |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Plans, open items and known limitations. |
| [docs/SECURITY.md](docs/SECURITY.md) | Security model and limits. |
| [TEST_REPORT.md](TEST_REPORT.md) | What has been verified and what hasn't. |
| [DEPENDENCIES.md](DEPENDENCIES.md) | Dependency inventory. |

Bot repository ([adrian-discord-voice](https://github.com/Riqqqque/adrian-discord-voice)): [README](https://github.com/Riqqqque/adrian-discord-voice#readme), [CONTRIBUTING](https://github.com/Riqqqque/adrian-discord-voice/blob/main/CONTRIBUTING.md), [usage](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/usage.md), [tools](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/tools.md), [costs](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/costs.md), [PC app setup](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/pc-app.md), [satellite protocol](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md), [roadmap](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/roadmap.md), [design decisions](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/decisions.md) (the PC app's are under [The PC app and PC actions](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/decisions.md#the-pc-app-and-pc-actions)).

## License

See [LICENSE](LICENSE).
