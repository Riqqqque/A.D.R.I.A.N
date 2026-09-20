# A.D.R.I.A.N

**A**dvanced **D**igital **R**easoning & **I**ntelligence **A**ssistant **N**etwork

A self-hosted household assistant: authenticated local web UI, real AI
provider chat with typed tools, Home Assistant light control, durable memory,
and alarms that fire on configured audio endpoints — all backed by a local
SQLite database.

**Status: Gate A preview + early Gate B.** The server, web UI, provider
adapters, Home Assistant integration, memory, alarm scheduler, and the
satellite pairing + push-to-talk voice path are implemented and tested.
Still unbuilt: local wake word, tray UI, and satellite-side alarm audio.
See `TEST_REPORT.md` for what is actually verified versus skipped.

## Requirements

- Rust stable (developed on 1.94, MSVC toolchain) + a C compiler
  (Visual Studio Build Tools on Windows) for bundled SQLite.
- Node.js 18+ (developed on Node 24) for the frontend build.
- Optional: a Home Assistant instance with a long-lived access token.
- Optional: an OpenAI, DeepSeek, or local OpenAI-compatible inference endpoint.

## Run it

```sh
# 1. Build the web UI (once, or after frontend changes)
cd web
npm ci
npm run build
cd ..

# 2. Build and start the server
cargo build --release -p adrian-server
set ADRIAN_DATA_DIR=%USERPROFILE%\.adrian      # or: ADRIAN_DATA_DIR=~/.adrian on bash
.\target\release\adrian-server.exe             # or ./target/release/adrian-server
```

On first boot the server prints a **one-use setup token** to the console and
saves it to `SETUP_TOKEN.txt` in the data directory. Open
`http://127.0.0.1:8787`, paste the token, and create the owner account.

Then, in the UI:

- **Integrations** — add an AI provider (API keys stay server-side) and/or
  connect Home Assistant (URL + long-lived token), then *Import entities*.
- **Devices** — allowlist the light(s) Adrian may control. Nothing is
  controllable until you check the box.
- **Audio** — click *Use this browser as a speaker* (tab must stay open) or
  add a `media_player.*` endpoint for Home Assistant playback.
- **Alarms** — schedule timers/alarms; they are committed to disk before the
  UI confirms and do not need the model or internet at due time.
- **Memory** — save/search/edit/delete facts. Private facts are invisible to
  other users.
- **System** — diagnostics, action ledger, settings, online backup.

## Voice satellite (Windows)

```sh
cargo build --release -p adrian-satellite

# In the web UI: Devices → Voice satellites → Generate pairing code
.\target\release\adrian-satellite.exe pair --server http://127.0.0.1:8787 --code 123456 --name "Desk PC"
.\target\release\adrian-satellite.exe run      # hold Ctrl+Alt+Space, speak, release
.\target\release\adrian-satellite.exe status   # verify the device token
.\target\release\adrian-satellite.exe say "test"  # verify speech output
```

How it works: the satellite captures microphone audio while the hotkey is
held, POSTs the WAV to `/api/satellite/utterance` with its device token, and
the server transcribes it via the configured provider's OpenAI-compatible
`/audio/transcriptions` endpoint (works against OpenAI or a local whisper
server), runs the normal assistant turn, and returns reply text which the
satellite speaks locally (SAPI on Windows). Pairing codes are one-use and
expire in 5 minutes; device tokens are sha256-hashed at rest and revocable
from the Devices page. Strict-local privacy mode blocks cloud STT egress.

Honest limits: push-to-talk only (no wake word yet), one utterance per
request (no streaming session protocol yet), and satellite audio output is
local TTS — satellites are not yet alarm playback endpoints.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `ADRIAN_DATA_DIR` | `./data` | database, backups, setup token |
| `ADRIAN_BIND` | `127.0.0.1:8787` | bind address (`0.0.0.0:8787` for LAN) |
| `ADRIAN_WEB_DIR` | `web/build` | built frontend location |
| `RUST_LOG` | `info,adrian=debug` | log filter |

There is no built-in TLS. For LAN use, prefer a TLS-terminating reverse
proxy. See `docs/SECURITY.md` for the security model and honest limitations
(including: data at rest is **not** encrypted in this build).

## Architecture

```
crates/protocol   shared wire types for server <-> satellite
crates/server     axum HTTP API + SQLite worker + providers + HA + scheduler
crates/satellite  native Windows voice satellite (push-to-talk, SAPI output)
web/              SvelteKit SPA, statically built, served by the server
```

Design rules enforced by the code:

- LLMs propose typed actions; the server validates scope, entity, action, and
  parameters before executing. There is no unrestricted HA service passthrough
  and no model-invoked shell.
- Physical actions are reported as *accepted* vs *confirmed* — a state change
  is only "confirmed" when the HA state subscription observes it.
- Alarms are persisted before acknowledgment; delivery rows deduplicate by
  `(alarm_id, occurrence)`; a server restart reconciles ambiguous deliveries
  honestly instead of silently refiring.
- All SQLite access runs on one bounded worker thread; WAL + `synchronous=FULL`.
- No telemetry. Strict-local mode refuses non-loopback provider endpoints.

## Verify

```sh
cargo fmt --all --check
cargo clippy --workspace
cargo test --workspace
cd web && npm run check && npm run build
```

Results and environment details: `TEST_REPORT.md`.
Dependency inventory: `DEPENDENCIES.md`.
