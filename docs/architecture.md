# Architecture: the crates in this repo and how they connect

This repo is a Cargo workspace with two separate stacks in it. Only one of them is
what A.D.R.I.A.N. users run today. Read this page first so you know which code you
are looking at.

| Stack | Crates | Status | Talks to |
|---|---|---|---|
| **The PC app** (shipping) | `crates/desktop` (`adrian-desktop`, the Windows app `ADRIAN.exe`), `crates/pc-control` (`adrian-pc-control`, PC actions and the `pcctl` CLI) | Released and auto-updating. Version tracks the bot release number (0.17.1 at the time of writing). | The bot's satellite WebSocket (`/satellite`) in the bot repo, [adrian-discord-voice](https://github.com/Riqqqque/adrian-discord-voice). Updates come from that repo's GitHub releases. |
| **The older household server** | `crates/server` (`adrian-server`), `crates/satellite` (`adrian-satellite`), `crates/protocol` (`adrian-protocol`), `web/` (SvelteKit UI) | Maintained code with tests (last server changes 2026-11-01), all at version 0.1.0. Not deployed and not what the PC app uses. | Its own web UI, its own old Windows client (`crates/satellite`), OpenAI-style providers, Home Assistant. Nothing in it calls the bot, and the bot does not call it. |

The brain of A.D.R.I.A.N. (speech recognition, the wake word, the language model, tools,
the voice, music, memory, alarms and budgets) lives in the bot repo. The PC app is a
thin, low-footprint client for it: a microphone, a speaker, an orb window and a set of
allowlisted PC actions. All AI cost is paid on the bot server, never on the PC.

Before you invest time in `crates/server`, `crates/satellite`, `crates/protocol` or
`web/`, check with the maintainer whether the work belongs there or in the bot.

## Contents

- [The shipping path](#the-shipping-path-pc-app-and-bot)
- [The older path](#the-older-path-rust-household-server)
- [Same names, different things](#same-names-different-things)
- [Workspace facts](#workspace-facts)
- [How crates depend on each other](#how-crates-depend-on-each-other)
- [A spoken request at the PC, end to end](#a-spoken-request-at-the-pc-end-to-end)
- [A PC action, end to end](#a-pc-action-end-to-end)
- [Releases and versions](#releases-and-versions)
- [Where data lives](#where-data-lives)
- [Cross-repo couplings](#cross-repo-couplings-change-them-together)
- [Build and test](#build-and-test)

Related pages: [desktop.md](desktop.md) (PC app internals), [pc-control.md](pc-control.md)
(every PC action and its safety rules), [SECURITY.md](SECURITY.md), [ROADMAP.md](ROADMAP.md),
[../TEST_REPORT.md](../TEST_REPORT.md), [../DEPENDENCIES.md](../DEPENDENCIES.md),
[../CONTRIBUTING.md](../CONTRIBUTING.md). Bot side of the link:
[satellite.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md) (the
wire contract) and [pc-app.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/pc-app.md)
(install and use).

## The shipping path: PC app and bot

```
 Windows PC                                         Bot server (adrian-discord-voice, Node)
 +--------------------------------------+           +-------------------------------------+
 | ADRIAN.exe  (crates/desktop, Tauri 2)|           | satellite WebSocket  /satellite     |
 |                                      |  hello    |   one "room" per connected PC       |
 |  mic -> loudness gate ---------------+---------->|   VAD, wake phrase, ASR             |
 |         (only while someone talks)   |  PCM up   |   language model + tools            |
 |                                      |           |   TTS voice, music, alarms, budgets |
 |  speakers <- playback queue <--------+<----------|   PCM down, state, transcript       |
 |  orb window, tray, settings          |           |                                     |
 |                                      |  action   |   pc_* tools -> action to the       |
 |  crates/pc-control  <----------------+<----------|   asker's own PC                    |
 |    (checks this PC's policy)  -------+---------->|   action_result                     |
 |                                      |           +-------------------------------------+
 |  updater  <---- GitHub releases of the bot repo (exe + .sha256)                        |
 +--------------------------------------+
```

- One process, `ADRIAN.exe`, built from `crates/desktop`. It is a tray app; the window
  (the animated orb, transcript, typing box and settings) only exists while it is open.
- The PC connects out to the bot at `ws://<host>:<port>/satellite` (default port 8791 on
  the bot side). The first frame is `hello` with a `sat_` pairing token. Until the server
  answers `welcome`, the PC sends only `hello`, `ping` and "Not connected yet." refusals:
  it drops its own audio and input, refuses `action` and ignores `alarms`. Status,
  transcript and audio messages from the server are handled from the start.
- The mic is always open, but audio leaves the PC only while a local loudness gate says
  someone is talking (48 kHz mono 16-bit PCM, about 96 KB/s while open). The server does
  the exact speech detection, the wake phrase ("Adrian"), transcription and everything after.
- The bot treats each connected PC as its own room with the same brain, voice, memory
  and budgets as its Discord call.
- When the model wants to do something on a PC it calls one of nine `pc_*` tools. The
  bot sends an `action` only to the asker's own connected PC; `crates/pc-control` checks
  the PC's own policy and does it. The server cannot change that policy.
- Details: [desktop.md](desktop.md) and [pc-control.md](pc-control.md).

## The older path: Rust household server

```
 browser (web/, SvelteKit) --HTTP + SSE--> adrian-server (axum + SQLite) --> OpenAI-style providers
                                               |   ^                    --> Home Assistant (lights only)
 adrian-satellite (old Windows client) --WS----+   |
   uses crates/protocol envelopes            pairing, /api/satellite/session
```

- `crates/server`: axum HTTP API, SQLite on one worker thread, an authenticated web UI,
  chat with typed tools, Home Assistant light control, memory (facts + FTS5), durable
  alarms, usage metering and monthly budgets, secrets encrypted at rest, optional TLS.
  Security model: [SECURITY.md](SECURITY.md#the-older-rust-household-server).
- `crates/satellite`: the first Windows voice client (console + tray, push-to-talk,
  optional rustpotter wake word, Windows SAPI voice). It talks only to `crates/server`.
  `crates/desktop` replaced it for real use; it shares no code with it. Whether to remove
  it is an open decision; don't spend time fixing it before that is settled.
- `crates/protocol`: the versioned JSON envelopes between those two only
  (`PROTOCOL_VERSION = 1`). The PC app and the bot do not use it.
- `web/`: the server's UI. Built output is served by the server; after `npm run build`
  restart the server, because the CSP hash of the inline boot script is computed at start.

All of these are built by `cargo test --workspace` and `cargo clippy --workspace`, so they
must keep compiling even if you never touch them.

### Older server: files

None of this is in production. Paths are relative to `crates/`. The one-line roles come
from each file's `//!` header.

| Path | Role |
|---|---|
| `server/src/main.rs` | Entry point: config, state, router, listener (optional TLS). |
| `server/src/lib.rs` | Builds `AppState` and the HTTP router. |
| `server/src/config.rs` | Config from env/CLI (`ADRIAN_*`); never holds secrets. |
| `server/src/state.rs` | `AppState` (db, rate limiter, per-user `TurnGates`, HTTP client, `ServerEvent` broadcast, scheduler, audio registry, satellite hub, HA manager, secret box). |
| `server/src/error.rs` | `ApiError`: every failure is a bounded JSON error. |
| `server/src/util.rs` | CSPRNG ids/tokens, hashing, time helpers. |
| `server/src/web.rs` | Serves the built SvelteKit UI with SPA fallback, security headers and the CSP hash of the inline boot script. |
| `server/src/audio.rs` | Browser playback channels (SSE), short-lived media grants, delivery acks, built-in alarm tone. |
| `server/src/secrets.rs` | ChaCha20-Poly1305 encryption of the `secrets` table (`enc:v1:`); key file DPAPI-wrapped on Windows. |
| `server/src/usage.rs` | Usage events, prices and monthly budgets; local endpoints are free and never blocked. |
| `server/src/auth/mod.rs` | One-use setup token, sessions in HttpOnly SameSite=Lax cookies, CSRF token, `AuthUser`, `client_ip` (forwarded headers only via `ADRIAN_TRUSTED_PROXY`). |
| `server/src/auth/password.rs` | Argon2id hashing and verification (PHC strings). |
| `server/src/auth/ratelimit.rs` | Sliding-window `RateLimiter` for sensitive routes; `TurnGates` so one user's assistant turns don't overlap. |
| `server/src/db/mod.rs` | SQLite on one dedicated worker thread behind a bounded channel. |
| `server/src/db/migrate.rs` | Ordered, transactional schema migrations (`schema_migrations`). |
| `server/src/providers/mod.rs` | Provider contract (`ChatProvider`, `ToolSpec`, `ChatEvent`), provider rows, `base_is_local`. |
| `server/src/providers/openai_compat.rs` | Chat Completions adapter (DeepSeek, local llama.cpp `/v1`, any compatible endpoint). |
| `server/src/providers/openai_responses.rs` | OpenAI Responses API adapter (`POST /v1/responses`, streaming SSE). |
| `server/src/tools/mod.rs` | Typed tools the model may propose; parse, scope, validate, permission check, execute, verify, record. No generic HA service tool. |
| `server/src/tools/fastpath.rs` | Unambiguous commands skip the model but go through the same policy checks. |
| `server/src/ha/mod.rs` | Home Assistant REST commands plus a supervised WebSocket state subscription. |
| `server/src/memory/mod.rs` | Owner-scoped facts with provenance, FTS5 index. |
| `server/src/scheduler/mod.rs` | Durable alarm scheduler: commit first, deduplicated deliveries, restart reconciliation. |
| `server/src/scheduler/replicas.rs` | Mirrors alarms routed to a satellite as `AlarmReplica`s (`AlarmReplicaSync` on connect and on changes). |
| `server/src/api/mod.rs` | Merges the route groups below; only setup/login and tokenized media are public. |
| `server/src/api/chat.rs` | `/api/chat`: fast path, then a bounded model/tool loop; conversation history. |
| `server/src/api/alarms.rs` | `/api/alarms`: list, create, snooze, stop, cancel. |
| `server/src/api/devices.rs` | `/api/devices/entities`: inventory, allowlist, light commands through the same gate as the tool. |
| `server/src/api/diagnostics.rs` | `/api/health`, `/api/diagnostics` (status, backup, restore, action ledger). |
| `server/src/api/endpoints.rs` | `/api/endpoints` and `/api/audio/*`: HA media players and opt-in browser audio outputs. |
| `server/src/api/events.rs` | `/api/events`: SSE stream of entity, alarm and HA status events for signed-in clients. |
| `server/src/api/integrations.rs` | `/api/integrations/*`: AI providers and the HA connection; secrets are stored, never returned. |
| `server/src/api/media.rs` | `/media/{token}`: tokenized alarm sounds for players that can't hold a session. |
| `server/src/api/memory_api.rs` | `/api/memory`: list, search, create, edit, delete facts, scoped by visibility. |
| `server/src/api/satellites.rs` | `/api/satellites*` and `/api/satellite/*`: one-use pairing codes, hashed device tokens, the `/api/satellite/session` WebSocket (speech goes to the provider's `/audio/transcriptions`). |
| `server/src/api/settings.rs` | `/api/settings`: anyone signed in reads, only the owner writes. |
| `server/src/api/usage_api.rs` | `/api/usage*`: usage, prices and budgets (owner only). |
| `server/tests/http_flow.rs` | Real router and SQLite in a tempdir, driven with `tower::ServiceExt`. |
| `server/tests/usage_flow.rs` | Mock OpenAI-compatible SSE provider; metering and budget refusal. |
| `server/tests/ws_session.rs` | Satellite WebSocket: bearer auth, envelopes, PCM capture, alarm delivery. |
| `satellite/src/main.rs` | The old console voice client: push-to-talk, WebSocket session, SAPI voice. |
| `satellite/src/wake.rs` | Optional rustpotter wake word (feature `wake-word`); off unless `wake_word.model_path` points at a readable `.rpw` file. |
| `satellite/src/replicas.rs` | Alarm replicas kept in `replicas.json` beside the satellite config. |
| `satellite/src/tray.rs` | Tray icon on its own thread with a win32 message loop. |
| `protocol/src/lib.rs` | Versioned `Envelope` messages between server and satellite (`PROTOCOL_VERSION = 1`). |

## Same names, different things

| Name | In the PC app (shipping) | In the older stack |
|---|---|---|
| "satellite" | The PC app's link to the bot (`/satellite` on the bot server) | `crates/satellite`, paired with `crates/server` over `/api/satellite/session` |
| `sat_...` token | Issued by the bot (`adrian satellite add` there), checked by the bot | Issued by `crates/server` after a 6-digit pairing code |
| Settings folder | `%APPDATA%\adrian\desktop\config\` (`config.json`, `alarms.json`) | `%APPDATA%\adrian\satellite\config\` (`config.json`, `replicas.json`) |
| `orb.js` | `crates/desktop/ui/orb.js` (embedded in the exe) | `crates/satellite/ui/orb.js` and `preview.html`: an earlier copy, not used by the app, and different |
| Ctrl+Alt+Space | Push-to-talk in the PC app | Push-to-talk in the old satellite |
| Alarm backup ring | `crates/desktop/src/alarm.rs`, fed by the bot's `alarms` message | `crates/satellite/src/replicas.rs`, fed by `AlarmReplicaSync` |
| `adrian_session` cookie | The bot's admin web UI uses this name for its own sessions | `crates/server` web sessions |

None of these are shared at runtime. A token from one stack never works on the other.

## Workspace facts

| Item | Value | Where |
|---|---|---|
| Members | `crates/protocol`, `crates/server`, `crates/satellite`, `crates/pc-control`, `crates/desktop` | `Cargo.toml` |
| Edition / MSRV | edition 2024, `rust-version = "1.88"` | `[workspace.package]` |
| License field | `license = "Apache-2.0"` in `[workspace.package]`; the `LICENSE` file is MIT. The two disagree; this is an open item for the maintainer. | `Cargo.toml`, `LICENSE` |
| Release profile | `lto = "thin"`, `codegen-units = 1` | `Cargo.toml` |
| C runtime | Linked statically for `x86_64-pc-windows-msvc` (`+crt-static`), so `ADRIAN.exe` needs no Visual C++ Redistributable | `.cargo/config.toml` |
| Lockfile | `Cargo.lock` is committed | repo root |
| Tauri pin | `tauri = "=2.11.6"`, `tauri-build = "=2.6.3"`: the newest that keep Rust 1.88 (2.12 needs 1.90) | `crates/desktop/Cargo.toml` |
| `windows` crate | 0.58.0 in both `desktop` and `pc-control`; keep them in step | both `Cargo.toml` files |
| CI | GitHub Actions on push and pull request to `main`: Windows clippy (`-D warnings`) and tests for the whole workspace, Linux clippy and tests for `adrian-protocol` and `adrian-server`, and the MSRV 1.88 check on Windows. Not yet run on GitHub when this was written; the Linux job is unverified. What it doesn't cover: [CONTRIBUTING.md](../CONTRIBUTING.md#continuous-integration) | `.github/workflows/ci.yml` |

## How crates depend on each other

```
adrian-desktop  ---> adrian-pc-control          (path dependency; the only link between PC crates)
adrian-server   ---> adrian-protocol
adrian-satellite ---> adrian-protocol
```

No other crate-to-crate links exist. `adrian-desktop` does not depend on `adrian-protocol`,
and nothing in the PC crates calls `crates/server`.

## A spoken request at the PC, end to end

1. **Mic and gate (PC).** The default Windows input device is resampled to 48 kHz mono
   and cut into 20 ms frames. The gate opens after 60 ms above an automatic threshold and
   sends 300 ms of audio from just before it opened (`speech_start`, binary PCM,
   `speech_end`). It closes after 1.2 s of quiet. Push-to-talk (Ctrl+Alt+Space) forces it
   open; from the next release the app sends the press with `hold: true` and the release
   too, and the bot takes only what was said in between. While the PC's owner is in a voice
   call the bot can see and the PC reports speakers (`output`), the bot takes only
   push-to-talk and typing there.
2. **Bot.** Silero VAD, wake phrase, ASR (local, optionally re-checked by a cloud model),
   the language model with tools, then the voice. See the bot's
   [architecture.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/architecture.md).
3. **Back to the PC.** `mode: thinking`, `transcript` lines (what was heard, her answer as
   it grows), `state {speaking, listenMs}`, and her audio as binary PCM.
4. **Playback (PC).** Resampled to the output device and played. If you talk over her, the
   gate can open over her echo and the PC drops her volume to 30 % at once; the server then
   stops her. The orb shows idle, listening, thinking or speaking from the server's state.

Typed text from the window goes up as one `text` message (up to 2000 characters), skips
the gate and the speech steps, and always counts as addressed to her.

## A PC action, end to end

1. The model calls a `pc_*` tool on the bot (`src/satellite/tools.ts`, `senses.ts` there).
   The bot rewrites a few arguments (`pc_open search` becomes a `url`, `youtube` + `play`
   becomes a `video_id`, and only the person's own spoken yes sets `confirmed` for sleep,
   restart and shutdown).
2. The bot sends `{"type":"action","id","name","args"}` to the asker's own PC.
3. The PC refuses any action that arrives before `welcome`. Otherwise it runs
   `Pc::run(name, args)` from `crates/pc-control` on a blocking thread, using the policy
   as it is at that moment.
4. `Pc::run` checks the policy (`allowed()` in `lib.rs`), then dispatches to `win::run`.
5. The PC answers `{"type":"action_result","id","ok","result"}`, plus `image` for
   `pc_screen`. The bot either speaks a fixed confirmation built from the result (saving a
   second model call) or gives the result to the model.

Every action, argument and safety rule is in [pc-control.md](pc-control.md).

## Releases and versions

- `crates/desktop/Cargo.toml` `version` and `crates/desktop/tauri.conf.json` `version` are
  kept equal and track the bot's release number. `pc-control`, `server`, `satellite` and
  `protocol` stay at 0.1.0.
- The PC app is released as assets of the bot repo's GitHub releases:
  `adrian-pc-app-<version>-windows-x64.exe`, its `.sha256`, and a `.zip` for first installs.
  The updater uses only the bare `.exe` and `.sha256`; a release without them strands every
  installed PC on its current version.
- The updater checks `https://api.github.com/repos/<owner>/adrian-discord-voice/releases/latest`
  3 minutes after start and every 6 hours, verifies the checksum, proves the download starts
  (`--self-check` must print the exact tag), installs only while the app is idle in the tray,
  and keeps the previous exe until the new one has been welcomed by the server and has run
  for 3 minutes. Details: [desktop.md](desktop.md#self-update).
- The tooling that builds, scans and uploads release assets is not part of either public
  repo. Write `tauri.conf.json` without a UTF-8 BOM (Windows PowerShell 5.1
  `Set-Content -Encoding utf8` adds one and that breaks the Tauri build).
- The release exe is not code-signed, so Windows SmartScreen warns on first run.

## Where data lives

| What | Where | Written by |
|---|---|---|
| PC app settings (server address, DPAPI-encrypted token, mic threshold, policy, switches) | `%APPDATA%\adrian\desktop\config\config.json` | `crates/desktop` |
| PC alarm backup copy, pending answers and answer markers | `%APPDATA%\adrian\desktop\config\alarms.json` | `crates/desktop` |
| PC link log (next release) | `%APPDATA%\adrian\desktop\config\link.log` and `link.old.log`, about 128 KB each | `crates/desktop` |
| PC app exe and update leftovers | the install folder (`%LOCALAPPDATA%\A.D.R.I.A.N\` with `Install.ps1`): `ADRIAN.exe`, `ADRIAN.new.exe`, `ADRIAN.old.exe` | installer, updater |
| PC action state | none on disk; app index and window undo history in memory | `crates/pc-control` |
| Everything the brain keeps (memory, people, alarms, usage, logs, pairing list) | the bot's state directory on the bot server | bot repo |
| Older server database, backups, key, setup token | `ADRIAN_DATA_DIR` (default `data/`): `adrian.db`, `backups/`, `secret.key`, `SETUP_TOKEN.txt` | `crates/server` |
| Old satellite config and alarm replicas | `%APPDATA%\adrian\satellite\config\` | `crates/satellite` |

The PC app's only log file is `link.log` (next release; 0.17.1 writes none): how each
connection ended and why tries failed, with the server's host and the token blanked
([desktop.md](desktop.md#linklog)). Its `eprintln!` output is invisible in release builds.

## Cross-repo couplings (change them together)

The PC crates and the bot are released together but live in two repos. These pieces must
match, or the bot misbehaves with PCs in the field. Older PC apps must keep working: add
fields, never repurpose or require them.

| PC side (this repo) | Bot side (adrian-discord-voice) | What must match |
|---|---|---|
| `crates/desktop/src/link.rs` message handling | `src/satellite/connection.ts`, `server.ts`, `docs/satellite.md` | Message types and fields both ways; frame size caps; `welcome` before any action, alarm or PC speech and input |
| `pc-control` tool and action names, argument names | `src/satellite/tools.ts`, `senses.ts` tool specs | Names and enums; old apps answer `Unknown PC action <name>.`, which the bot turns into "needs an update" |
| `win/open.rs` `web_url` (`MAX_URL` 300, `PRIVATE_SUFFIXES`, host rules) | `tools.ts` `webUrl` | Same rules (the bot copy has no DNS check) |
| `win/system.rs` `DELAY_SECS = 30` | `tools.ts` `POWER_DOWN_MS = 30_000` and the "30 seconds" in tool text | The cancel window for restart and shutdown |
| `win/screen.rs` `MAX_SIDE = 1568`, `TARGET_BYTES = 1_000_000` | `src/satellite/images.ts` `MAX_IMAGE_BYTES = 1_500_000`; 19 s / 20 s screen timeouts | Picture size limits |
| `win/clipboard.rs` `MAX_CHARS = 8000` | `senses.ts` description; the 9000-character result cap | Clipboard text size |
| Result wording: `Unknown PC action` prefix, `pc_apps open` "(app/game/shortcut)." suffix, `pc_clipboard` first line, `pc_screen` reply shape | `senses.ts` `UNKNOWN`, `forLog`; `tools.ts` `confirmation()` | Parsed by the bot; reword only together |
| `activity` message (`gaming`, `game` up to 60 characters) | `src/satellite/quiet.ts` `parseActivity` | Quiet while gaming |
| `alarm.rs` reading of `alarms` (`ringing`, `list`, `snoozeMin`, `ringMin`, `now`) and `alarm_action` / `alarm_done` | `src/app/alarm-commands.ts`, `connection.ts` | PC backup ring. From the next release `alarm_done` is sent at once on a live link, which relies on the bot accepting it at any time (it does) |
| `ptt {down, hold}` and `ptt {down: false}`; `output {kind}`; `notice {kind}` (`link.rs`, next release) | `src/satellite/ptt.ts`, the PC call guard in `src/app/stack.ts`, `connection.ts` | Held push-to-talk; hands-free on headphones during a call; the chime. All optional both ways: an old app sends no `hold` (one-shot press) and no `output` (treated as speakers); an old bot ignores both and sends no `notice` |
| `update.rs` `REPO`, asset names, tag format `v<X.Y.Z>` | release assets of the bot repo | Auto-update |
| `hello.version` (`CARGO_PKG_VERSION`) | `senses.ts` `HEALTH_SINCE` and other version checks | The bot degrades for older apps by version |

## Build and test

```
cargo build --release -p adrian-desktop        # target\release\adrian-desktop.exe (installed as ADRIAN.exe)
cargo test -p adrian-desktop                   # 116 tests (3 ignored measurements)
cargo test -p adrian-pc-control                # 109 tests (1 ignored live test)
cargo build -p adrian-pc-control --bin pcctl   # the PC actions CLI
cargo clippy --workspace
cargo fmt --all --check
cargo +1.88.0 check --workspace --all-targets  # MSRV
cargo test --workspace                         # also builds and tests the older server stack
cd web && npm ci && npm run check && npm run build && npm test
```

Test counts are `#[test]` / `#[tokio::test]` attributes on 2026-11-20 (desktop: 73 at
`ac9708d`, 0.17.1); see [../TEST_REPORT.md](../TEST_REPORT.md). The orb has no automated
test; `crates/desktop/bench/orb-bench.mjs` measures it in headless Edge
([desktop.md](desktop.md#change-guide)). Most PC crate tests only compile on Windows.
Never run anything that locks, sleeps, restarts or shuts down the machine as a test; see
[pc-control.md](pc-control.md#tests).
