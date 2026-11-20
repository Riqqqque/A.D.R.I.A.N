# Roadmap

This page covers this repository: the Windows PC app (`crates/desktop`), PC control (`crates/pc-control`) and the older Rust household server (`crates/server`, `web/`, `crates/satellite`, `crates/protocol`). It also keeps the long-term design plan for the household server.

The bot's plans (speed, cost, music, alarms, home control over MCP, the bot's known limitations) are in the bot repository's [docs/roadmap.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/roadmap.md). That page's PC section carries a short copy of the main PC items; this page is the full list. When you add, close or change a PC item here, check that page too. Why the PC app is built the way it is (and so which items are deliberate, not missing) is in the bot's [docs/decisions.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/decisions.md#the-pc-app-and-pc-actions).

State of the code: PC app 0.17.1 (2026-11-19). Page updated 2026-11-20. It replaces the design review of 2026-10-14, whose findings and plans are carried over below with their current status. Nothing here is a promise or a schedule.

Status words: **done** = in the code and tested; **open** = planned, not started; **superseded** = the plan was overtaken by what was built.

## Contents

- [Where things stand](#where-things-stand)
- [What changed since the 2026-10-14 review](#what-changed-since-the-2026-10-14-review)
- [Findings from the review, with their status](#findings-from-the-review-with-their-status)
- [PC app and PC control: next items](#pc-app-and-pc-control-next-items)
- [PC app and PC control: known limitations](#pc-app-and-pc-control-known-limitations)
- [Waiting on a decision](#waiting-on-a-decision)
- [The household server: design plan](#the-household-server-design-plan)
- [A possible order](#a-possible-order)
- [Taking on an item](#taking-on-an-item)

---

## Where things stand

| Part | State | Notes |
|---|---|---|
| PC app (`crates/desktop`) | **Live.** Released with every bot release since 0.14.9; updates itself from the bot repo's GitHub releases. | Talks to the bot's satellite WebSocket, not to the Rust server. 73 test attributes. |
| PC control (`crates/pc-control`) | **Live** through the PC app. | Nine PC tools, game detection, `pcctl`. 109 test attributes. |
| Discord bot and the PC app's server side | **Live**, in [adrian-discord-voice](https://github.com/Riqqqque/adrian-discord-voice). | The hub of the suite today: brain, memory, people, budgets, alarms, voices. |
| Household server (`crates/server`, `web/`) | Preview ("Gate A + early Gate B": Gate A is the server core, Gate B the old voice satellite; see the [README](../README.md#the-older-rust-household-server)). Maintained (last server changes 2026-11-01), not in production. | 157 test attributes. Never run against a real provider or Home Assistant ([TEST_REPORT.md](../TEST_REPORT.md)). |
| Old satellite (`crates/satellite`) and `crates/protocol` | Superseded by the PC app. | Kept until the maintainer decides ([below](#waiting-on-a-decision)). |

The vision is unchanged. A.D.R.I.A.N. is a personal assistant suite you run yourself, that others can run too. Each part can be local, cloud or mixed. It should be as cheap as possible to run, with monthly caps that hold. It is security first, private by default, and acts only through narrow, typed tools.

## What changed since the 2026-10-14 review

The review planned the Rust household server as the hub, with a Discord companion as its client. That companion was to be a Rust service using Songbird, talking to the server over a scoped client token and sharing its memory, persona and budgets. What was built instead:

- **The Discord bot came first, in TypeScript**, as an independent service with its own memory, alarms, usage ledger and spend caps, and Home Assistant access through Home Assistant's MCP server. It became the hub. The review's §11 (Discord companion) and the first item of its suggested order are **superseded**.
- **PC satellite v2 (§8 of the review) was built** as `crates/desktop` plus `crates/pc-control`, and it **pairs with the bot**, not with this server. It differs from the plan in a few ways:
  - **Wake detection** happens on the server, from transcripts. The PC runs only a loudness gate with a 300 ms pre-roll, not a local keyword spotter (the plan suggested openWakeWord or sherpa-onnx keyword spotting).
  - **The orb** is a 2D canvas renderer (`ui/orb.js`), not WebGL. It stops drawing when off screen, and in the tray there is no window at all.
  - **PC control** is nine typed tools with a per-PC allow-list that the server can't change. It does more than the planned `open_app` / `open_url` / media / volume / lock set: windows, health, clipboard, screen pictures, power actions behind a spoken yes, and quiet-while-gaming detection.
  - **Measured footprint in the tray**: about 5 MB private memory, 0.01% CPU, 0% GPU (2026-10-23; 4.7 to 4.9 MB after a restart for releases 0.15.3 to 0.17.1).
- **The household server gained** roles and capabilities (default-deny, guest = chat only), trusted-proxy support, fact edit history, exact-match light commands, honest Home Assistant state, revocation that ends live sessions, and partial-turn reports (2026-11-01). Its own feature plans (§3 to §10 below) are still open.
- **Docs stay in the repositories.** The earlier idea of moving them to a GitHub wiki was dropped.

## Findings from the review, with their status

| # | Finding | Status (2026-11-20) |
|---|---|---|
| F1 | A paired satellite acts with its owner's **full role**. Anyone talking near the PC has the owner's authority. | **Open, and it applies to the PC app too.** On the bot, everything said at a PC counts as the person it was paired to. The stopgaps are the PC's own allow-list and the spoken yes for sleep, restart and shutdown. The fix is per-request assurance levels ([Identity](#identity-admin-and-assurance-levels)). |
| F2 | Rooms aren't first-class (no room on endpoints, satellites or people). | **Open** for this server. The bot has rooms per Discord call and per PC. |
| F3 | The protocol had no catch-all variant. | **Done** 2026-10-14 (`#[serde(other)] Unknown` in `crates/protocol`). The PC app ignores unknown server messages the same way. |
| F4 | No server-side TTS. | **Open** for this server. The bot has local Kokoro and Piper voices and an optional cloud voice. |
| F5 | Memory is facts plus full-text search only. | **Open** for this server ([Memory v2](#memory-that-lasts)). The bot has per-person notes that update and automatic memory. |
| F6 | No usage or cost ledger. | **Done** 2026-10-14: ledger, prices and monthly budgets. Still open: falling back to a local model at a hard cap instead of refusing. |
| F7 | Backups are manual. | **Open.** Plan: scheduled online backups with retention (daily ×7, weekly ×4, monthly ×12), plus an optional off-box copy. |
| F8 | Only the secrets table is encrypted. | **Open.** Plan: SQLCipher for the whole database once its build is proven on MSVC, with the key wrapped like `secret.key`. |
| F9 | The satellite is a console process with a tray icon. | **Done differently**: the PC app (`crates/desktop`) replaced it, but for the bot. |
| F10 | Only the next occurrence of a recurring alarm is replicated to satellites. | **Open** for the old satellite. The PC app gets the bot's alarm list for the next 8 days (at most 64 entries kept). |

## PC app and PC control: next items

Concrete items, in no fixed order. Each change here also needs the bot side checked (see [CONTRIBUTING.md](../CONTRIBUTING.md), ground rule 7).

### Safety and rights

- **Rights at a PC (F1).** Long-term: speaker verification as a soft factor plus assurance levels. Until then, the per-PC allow-list and the power yes stay the boundary.
- **Close the game-protection gaps** in `crates/pc-control`, so the code matches the "nothing that messages a running game" rule in the `lib.rs` header:
  - `pc_volume` with a named `app`, and the master volume, have no game check (only `playing: true` skips games);
  - `pc_system screen_off` broadcasts `WM_SYSCOMMAND` to every top-level window, a running game's included;
  - `pc_apps show_desktop` uses the shell's Show-desktop toggle, which minimizes every window, a game's included;
  - `pc_media` has no game check (it acts on media sessions only).
- **Game-folder roots without a trailing backslash** (Steam, Riot, Epic, the Vanguard folder): a sibling folder whose name only starts the same way would count as a game folder. Battle.net and the convention roots already end in `\`.
- **Release signing** (decision below). Today an update is trusted because its checksum comes from the same GitHub release.

### Reliability and speed

- **App index:**
  - `Pc::refresh()` has no caller, so a newly installed app appears only after the 10-minute rebuild on next use;
  - the first index-using action after start, or after a settings Save (which builds a new `Pc`), waits up to 15 s for the background build and then builds the index itself, so it can take longer than the bot's 15 s tool timeout;
  - games from Battle.net, Xbox, EA, Ubisoft and Rockstar folders have no launcher entry. They open by name only if the Start menu lists them, and they're missing from the no-filter `pc_apps list`.
- **Hard-coded paths**: Epic and Riot read literal `C:\ProgramData\...` paths and the Vanguard root is literal, so installs in other locations aren't seen.
- **Speaker output**: a `try_lock` on the output path can play a moment of silence under contention.
- **Orb cost with the window open** at high refresh rates (120 Hz) hasn't been measured or tuned. The tray state is already near zero.
- **Settings that reset on reconnect** (voice volume, speed, music level, queue): they live in the bot's per-connection PC room. The fix is on the bot side (keep them per device).
- **Default-device switch**: audio queued at the moment of a switch is dropped (`open_streams` clears the queue). Audio that arrives after the switch plays on the new device.

### Development

- **No CI.** Nothing runs `cargo test --workspace`, `cargo clippy --workspace` or the MSRV check automatically; they are run by hand.
- **Dev builds update themselves.** `update::run` isn't gated by build type, so a local build older than the latest release replaces itself about 3 minutes after start unless auto-update is off.
- **`Install.ps1`** has no automated test.
- **`pcctl --policy`** silently accepts the whole settings file as an all-on policy (`Policy` has no `deny_unknown_fields`).
- **Stale comments** listed in [CONTRIBUTING.md](../CONTRIBUTING.md#known-stale-comments).
- **`crates/desktop/src/audio.rs`** is 977 lines; split it only if that is clearly better.

### Live checks still to do

These paths have unit tests but little or no real-world use yet:

- window moves across monitors with different scaling;
- real screen captures;
- quiet while gaming with real games;
- raising quiet or muted speakers for an alarm ring.

## PC app and PC control: known limitations

What doesn't work, or works with a catch, as of 0.17.1:

- **The link is plain `ws://` by default**: a bare `host:port` or `http://` address becomes `ws://`, so the token and mic audio are unencrypted unless the server is behind TLS and the address is `wss://`/`https://`. Use a LAN, a VPN or a TLS reverse proxy. Tokens don't expire.
- **Windows only.** Other platforms compile, but the token can't be saved and every action answers "PC control only works on Windows."
- **No acoustic echo cancellation.** Talking over her relies on a learned speaker-to-mic coupling and a 3× margin. `examples/aec_probe.rs` explored Windows' communications-mode echo cancellation; it wasn't adopted.
- **Push-to-talk sends only the press** (`ptt down: true`), never a release; the gate closes on its own after 1.2 s of quiet.
- **`is_protected("")` is true**: elevated or otherwise unreadable windows are refused as "might be a game", even ordinary admin tools. This is deliberate, since unreadable exes are often anti-cheat-protected games.
- **`pc_windows` refuses every change while an exclusive full-screen D3D app runs**, even on another monitor. Window undo history is in memory only (32 entries).
- **`pc_open`** refuses every raw IP address, public ones too. The DNS check resolves once, and the browser resolves again later.
- **`pc_apps running` sends window titles to the model** (first 60 characters each, which can name documents or chats). `pc_screen` deliberately names only the app.
- **No CPU temperature** (Windows shows it only to admin tools); GPU stats are NVIDIA only.
- **The exe isn't code-signed**: SmartScreen warns on first run.
- **A corrupt `config.json` silently loads as defaults** (unpaired), and the next Save overwrites it.
- **Status text and messages are English-only** strings in code.
- **Dev builds share the installed copy's state**: settings, alarms and the single-instance event.

## Waiting on a decision

Product calls for the maintainer. Each has a working default today.

| Question | Today |
|---|---|
| Remove `crates/satellite`? | Kept. Superseded by the PC app, shares no code with it. Don't fix or extend it in the meantime. |
| Keep investing in `crates/server`, or fold its plans into the bot? | The server is maintained but not in production; the bot is the hub. Ask before starting server work. |
| Sign releases? | SHA-256 files from the same GitHub release. Signing would stop a hijacked publishing account from reaching every auto-updating PC. |
| License metadata | `LICENSE` is MIT; `[workspace.package] license` in `Cargo.toml` says Apache-2.0. To be settled by the maintainer. |

---

## The household server: design plan

This is the long-term plan for `crates/server` from the 2026-10-14 review, kept because it is still the direction for a home assistant. None of it has been started. Several parts now exist in the bot in a simpler form (see the table at the end of this section). Before building any of it, decide whether it belongs in this server or in the bot.

### What already holds up

New pieces should follow these patterns:

- **Typed tools, server-side policy.** The model proposes; the server checks role, scope, entity, action and parameters. Nothing passes through to Home Assistant unchecked, and the model can't run a shell. The PC app follows the same pattern.
- **Durable, honest scheduling.** Alarms are committed before they're confirmed, deduplicated per occurrence and reconciled after a crash, and the ledger keeps "accepted" and "confirmed" apart.
- **The deterministic fast path.** Simple commands skip the model entirely. It is the biggest lever on cost and latency.
- **One SQLite worker in WAL mode.** Plenty at household scale, with simple backup and restore.
- **Strict-local mode** as the base for mixing local and cloud per capability.

### Identity, admin and assurance levels

Goal: the household admin is recognized by voice, rules can be tightened in conversation ("tonight, require a code for admin requests"), and ultimate control stays on the server.

- **Principals**: people (owner, adults, kids, guests), each with a role and optional voice and face enrollments.
- **Assurance level per request**, from how the request arrived:

  | Level | Comes from |
  |---|---|
  | `none` | an unknown voice, or a Discord user who hasn't been linked |
  | `device` | a paired satellite or PC, or a speaker in a known room |
  | `voice` | a speaker-verification match above threshold |
  | `code` | a voice match plus a spoken or typed code |
  | `session` | a signed-in web UI session |
  | `console` | the server's local CLI |

- **Each tool declares the level it needs.** For example: lights need `device`; locks, alarms for other people and memory deletion need `voice`; policy changes need `code` or `session`.
- **Policy changes by conversation can only tighten.** "Require a code tonight" raises the bar and expires at a set time. Loosening anything (removing a code, adding an admin, lowering thresholds) needs `session` or `console`, so a prompt injection, an overheard voice or a replayed recording can never lower it.
- **Codes** are stored hashed (Argon2), rate-limited, and can be one-time or expiring. Spoken codes are easy to overhear, so they're a second factor, never the only one.
- **Break-glass**: an `adrian-server admin ...` CLI on the server resets policy, revokes devices, resets the owner password, and switches on a safe mode with no physical actions. The model can't reach it.
- **Speaker verification**, local: speaker embeddings (ECAPA-style, via sherpa-onnx), enrolled from about 30 s of speech. A soft factor that raises assurance and never authenticates alone.
- **Persona**: the assistant's name, how it addresses each person, and its tone are per-user settings, never hard-coded.

This is also the real fix for F1 on the PC app.

### Rooms, people and targeted announcements

- **Tables**: `rooms` (synced from Home Assistant areas, editable), a `room_id` on endpoints, satellites, cameras, entities and people, and `groups` ("kids" = these people).
- **Target resolution**: "me" means the speaker's room, or the room they're in if presence is known. "The kids" resolves through the group to their rooms; "upstairs" is a room group. Nothing broadcasts to the whole house unless asked for explicitly.
- **New tools**: `announce(target, text)` (speech to the resolved speakers only) and `set_alarm(target, ...)` (alarms carry a target, not just an endpoint).
- **Speech output**: Piper on the server renders a WAV, served through the existing short-lived `/media/{token}` grants to Home Assistant media players and satellites. Repeated phrases are cached.
- **Room hardware**: the Wyoming protocol, so cheap ESP32-based Home Assistant voice devices can be room satellites instead of a PC per room.

### Memory that lasts

- **Episodes**: a nightly job summarizes each day's conversations into a dated episode (who, what, decisions, follow-ups) with a cheap or local model.
- **Embeddings**: a local embedding model (bge-small via ONNX) and `sqlite-vec` in the same database. Recall combines full-text search, vector search and a time filter, so "what did we say about the car about a month ago" becomes a date window plus a semantic query.
- **Tools**: `recall(query, from?, to?, person?)` returns cited snippets with dates, and the model answers from those snippets.
- **Retention and privacy**: per-user private scopes carry over; add "forget this", "forget everything from date X", and export.

### Cost control

- **Ledger and budgets** (done): every provider call is recorded with tokens, audio seconds and cost from an editable price table; monthly caps per household, provider or module.
- **Hard caps that fall back** to a local or cheaper model instead of refusing (open).
- **A cost-ordered pipeline** (open):
  1. Fast path: deterministic intents with no model call (lights, timers, time, volume, scenes). Grow it aggressively.
  2. Small model: local or the cheapest cloud model for routine chat and tool choice.
  3. Large model: only on escalation (long reasoning, ambiguity, an explicit request).
- **Local voice I/O**: local speech-to-text and Piper remove per-minute charges, and voice-activity detection gates everything so silence costs nothing.
- **Prompt caching**: keep the system prompt and the tool list stable and first, so cached prefixes apply.

### Local, cloud or mixed

Generalize the provider table into **per-capability routing**: chat, speech-to-text, speech output, embeddings and vision each get an ordered fallback chain (for example, local whisper, then a cloud transcription API). Strict-local applies per capability, so "local speech, cloud chat" is one setting. The budget layer sits in this path, so a hard cap can fall back to the local step instead of failing. This also fixes a current gap: speech-to-text today uses the chat provider, and sends the chat model's name to OpenAI-compatible providers.

### Cameras and vision

- **Frigate** (self-hosted NVR) as the vision backbone instead of running models inside A.D.R.I.A.N. It already does object detection, zones, snapshots and face recognition on modest hardware.
- "Where did I leave my phone?" queries Frigate events for the last phone sighting and its zone, attaches the snapshot, and sends one frame to a vision model only when the detector is unsure.
- Faces stay local, enrollment is opt-in per person (with extra care for children), and footage follows retention limits. Strict-local applies to every camera frame.
- Presence from cameras feeds target resolution for rooms.

### Home Assistant scope

Expand one domain at a time: switches and scenes, then climate, media, covers, and last locks and alarm panels. Each domain gets typed parameters, an allow-list and a minimum assurance level (locks at `voice` or higher). Physical outcomes keep the accepted-versus-confirmed pattern. Today only lights exist here.

### Before any guest accounts

The server has no API to create members or guests yet. Before adding one, scheduled alarms need a rights check at ring time, and a guest's chat must not spend the shared budget unchecked.

### Alarm gaps

- Stop, snooze and cancel release the delivery and stop browser speakers (`release_deliveries` in `scheduler/mod.rs`), but nothing tells a satellite or a Home Assistant media player to stop a sound it is already playing.
- An alarm routed to an endpoint that has since been disabled fails at due time ("no enabled audio endpoint at due time") instead of falling back to another endpoint.

### What exists in the bot already

| Plan | In the bot today |
|---|---|
| Home Assistant | Home control through Home Assistant's MCP server (0.16.0): only allow-listed tools, owners by default, a spoken yes before locks and doors. |
| Memory | Per-person notes that update or drop, plus automatic memory every few answers. |
| Cost | A spend ledger with daily and monthly caps, local phrases with no model call, Claude prompt caching, `brain.background` for a cheaper memory model. |
| Rooms | Rooms per Discord call and per PC. |
| Speech output | Local Kokoro and Piper voices, optional Speechify cloud voice. |

---

## A possible order

The maintainer sets priorities; this is one reasonable order, not a schedule:

1. **PC safety**: the game-protection gaps and the game-folder root check in `crates/pc-control`, because they guard people's game accounts.
2. **PC reliability**: app index refresh and first-use wait, the speaker `try_lock`, pairing refusal messages.
3. **Decisions** on `crates/satellite`, release signing and the license metadata.
4. **Identity and assurance levels**, which also fix F1 for the PC app, wherever the maintainer decides they belong (this server or the bot).
5. Then the household server plans: rooms and announcements with Piper, memory v2 and scheduled backups, Frigate, more Home Assistant domains and Wyoming room satellites.

## Taking on an item

1. Read [CONTRIBUTING.md](../CONTRIBUTING.md), especially the area table and the live-testing hazards.
2. For anything a PC does, check the bot side too: the tool spec, its confirmations and the protocol in the bot's [docs/satellite.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md). Older PC apps must keep working.
3. When an item here is done, remove it from this page in the same commit, and from the bot roadmap's PC section if it's listed there.
