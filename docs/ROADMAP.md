# Roadmap and design review

This review covers the code as of the secrets/TLS/replica commit, checked
against where A.D.R.I.A.N is headed: an autonomous home AI with normal
assistant skills, a PC satellite, and a Discord companion. Every module can
run locally, in the cloud, or mixed, and spending is capped.

Status words: **have** = in the code and tested; **gap** = needed for the
vision and not started; **change** = existing behavior I'd rework.

---

## 1. What already holds up

These parts are worth keeping as they are. The newer pieces should follow
the same patterns.

- **Typed tools, server-side policy.** The model proposes; the server
  validates scope, entity, action and parameters. Nothing passes through to
  Home Assistant unchecked and the model can't run a shell. Every new
  capability (PC control, cameras, announcements) should follow this pattern.
- **Durable, honest scheduling.** Alarms are committed before they're
  confirmed, deduplicated per occurrence, and reconciled after a crash.
  Satellites now keep local replicas. The ledger records "accepted" and
  "confirmed" as different outcomes.
- **The deterministic fast path.** Simple commands skip the LLM entirely.
  This is the single biggest lever on both cost and latency (see §6).
- **One SQLite worker in WAL mode.** That's plenty at household scale, and
  backup and restore are simple.
- **Strict-local mode.** The policy layer refuses cloud egress per request.
  This is the base for "mix and match local and cloud" (see §7).

## 2. Findings in the current code

| # | Finding | Why it matters | Fix |
|---|---|---|---|
| F1 | A paired satellite acts with its owner's **full role** (`sat_auth_user` maps the device to the user who paired it). | Anyone talking near the owner's PC has owner authority. This conflicts with the voice-admin model. | Assurance levels (§3). A device grants at most "device" assurance. Admin actions need more. |
| F2 | Rooms aren't first-class. Entities carry an HA `area` string, but endpoints, satellites and people have no room. | "Wake the kids at 6" can't be routed to one room's speaker. | Rooms, people and groups model (§4). |
| F3 | The protocol has no catch-all variant. A newer server message makes an older satellite log "bad control frame". | Every additive protocol change needs every satellite rebuilt at once. | Add `#[serde(other)] Unknown` to `Message` and ignore unknown types. Small change; do it before the next protocol addition. |
| F4 | No server-side TTS. Media players can only play the built-in tone. | "Tell the kids to get ready for school" needs spoken audio on a room speaker. | Piper (local, free, fast on CPU) as a TTS capability, with cloud TTS optional (§4, §7). |
| F5 | Memory is facts plus full-text search only. | It can't recall "what we talked about a month ago" unless a fact was saved. | Memory v2: episodes, embeddings, time-scoped recall (§5). |
| F6 | No usage or cost ledger. | Monthly spending caps are impossible without metering. | Usage ledger and budgets (§6). This is needed before a 24/7 Discord bot. |
| F7 | Backups are manual. | "Memory that lasts forever" needs backups that run without anyone remembering to. | Scheduled online backups with retention (daily ×7, weekly ×4, monthly ×12), plus an optional off-box copy to the home-server shares. |
| F8 | Only the secrets table is encrypted. | Memory and chat history are the most personal data in the system. | Keep the current scheme. Plan SQLCipher for the whole database once its build is proven on MSVC, with the key wrapped the same way as `secret.key`. |
| F9 | The satellite is a console process with a tray icon. | The vision calls for a proper app window. | PC satellite v2 (§8). |
| F10 | Only one occurrence of each recurring alarm is replicated to satellites. | A satellite offline for days rings only the next occurrence. | Send the next N occurrences (for example 48 hours' worth) in `AlarmReplicaSync`. |

## 3. Identity, admin and "god rights"

What's wanted: the household admin is recognized by voice, rules can change
in conversation ("tonight, require code 1234 for admin requests"), the
assistant addresses the admin by name, and ultimate control stays on the
server.

Design:

- **Principals:** people (owner, adults, kids, guests). Each has a role and
  optional voice and face enrollments.
- **Assurance level per request.** It's computed from how the request
  arrived:
  - `none`: unknown voice, Discord user who hasn't been linked.
  - `device`: a paired satellite, or a speaker in a known room.
  - `voice`: a speaker-verification match above threshold.
  - `code`: voice plus a spoken or typed code.
  - `session`: a logged-in web UI session.
  - `console`: the server's local CLI.
- **Each tool declares the assurance it needs.** For example: lights need
  `device`; locks, alarms for other people and memory deletion need `voice`;
  policy changes need `code` or `session`.
- **Conversational policy changes can only tighten.** "Require a code
  tonight" raises requirements and expires at a set time. Loosening
  anything (removing a code, adding an admin, lowering thresholds) needs
  `session` or `console`. A prompt injection, an overheard voice or a
  replayed recording can therefore never lower the bar.
- **Codes** are stored hashed (Argon2), rate-limited, and can be one-time or
  expiring. Spoken codes are easy to overhear, so treat them as a second
  factor, never the only one.
- **Break-glass.** An `adrian-server admin …` CLI on the server can reset
  policy, revoke devices, reset the owner password and toggle a "safe mode"
  that turns off all physical actions. The model can't reach it.
- **Speaker verification**, done locally: an ECAPA-style speaker embedding
  via sherpa-onnx, with enrollment from about 30 s of speech. It's a soft
  factor that raises assurance and never authenticates alone.
- **Persona:** the assistant's name, how it addresses each person, and its
  tone are per-user settings, never hard-coded.

## 4. Rooms, people and targeted announcements

- **Tables:** `rooms` (synced from Home Assistant areas, editable),
  `room_id` on endpoints, satellites, cameras and entities, a `room_id` on
  people ("the kids' room"), and `groups` ("kids" = these people).
- **Target resolution:** "me" → the speaker's room, or the room they're in
  if presence is known. "The kids" → the group → their rooms. "Upstairs" →
  a room group. Nothing broadcasts to the whole house unless that's asked
  for explicitly.
- **New tools:** `announce(target, text)` (TTS to the resolved speakers
  only) and `set_alarm(target, …)` (alarms carry a target, not just an
  endpoint).
- **TTS pipeline:** Piper on the server renders a WAV, which is served
  through the existing short-lived `/media/{token}` grants to Home Assistant
  media players and satellites. Repeated phrases are cached.
- **Room hardware:** use the Wyoming protocol so cheap Home Assistant voice
  devices (ESP32-based) can be room satellites, instead of a PC per room.

## 5. Memory that lasts

- **Episodes:** every conversation is already stored. Add a nightly job
  that summarizes each day into a dated episode (who, what, decisions,
  follow-ups) with a cheap or local model.
- **Embeddings:** a local embedding model (bge-small via ONNX, free and
  fast) and `sqlite-vec` in the same database. Recall combines full-text
  search, vector search and a time filter, so "what did we say about the
  car about a month ago" becomes a date window plus a semantic query.
- **Tools:** `recall(query, from?, to?, person?)` returns cited snippets
  with dates. The model answers from these snippets, not from guesses.
- **Retention and privacy:** per-user private scopes carry over. Add
  "forget this" and "forget everything from date X", and export.

## 6. Cost control (as cheap as possible)

- **Usage ledger:** every provider call records capability, model, input
  and output tokens, audio seconds and estimated cost, using an editable
  price table.
- **Budgets:** monthly caps per provider, per module (home, Discord,
  satellite) and per user. A soft cap warns. A hard cap downgrades to a
  local or cheaper model, or refuses with an honest message.
- **A cost-ordered pipeline:**
  1. Fast path: deterministic intents (lights, timers, time, volume,
     scenes). No model call at all. Grow this aggressively.
  2. Small model: local or the cheapest cloud model for routine chat and
     tool selection.
  3. Large model: only on escalation (long reasoning, ambiguity, explicit
     request).
- **Local voice I/O:** faster-whisper or whisper.cpp for speech-to-text and
  Piper for TTS remove per-minute charges. Voice-activity detection gates
  everything, so silence costs nothing.
- **Cloud prompt caching:** keep the system prompt and tool list stable and
  first so cached prefixes apply.

## 7. Local, cloud, or mixed

Generalize the current provider table into **per-capability routing**: chat,
STT, TTS, embeddings and vision each get an ordered fallback chain, for
example `local-whisper → openai-stt`. Strict-local applies per capability,
so "local STT and TTS, cloud chat" is one setting. The budget layer (§6)
sits in this path, so a hard cap can fall back to the local step instead of
failing.

## 8. PC satellite v2

- **App shell:** Tauri 2 (Rust core plus a WebView2 window that's already on
  Windows 11): about 10 MB and low RAM, compared with roughly 150 MB for an
  Electron app. The tray click opens the window.
- **Animation:** a WebGL particle or "neural" orb driven by real state:
  listening (mic level), thinking, speaking (output level), idle. Keep it
  understated. Rendering stops when the window is hidden, so idle cost is
  zero.
- **PC control, typed and allowlisted on the satellite itself:**
  - `open_app(name)`: from an indexed Start-menu list plus a user allowlist,
    e.g. a game launcher.
  - `open_url(url)` and `youtube_search(query)` (opens a results or video
    URL).
  - Media keys, volume, lock screen.
  - No arbitrary shell, no keystroke injection.
  - The server picks the tool; the satellite enforces its own allowlist as
    a second check.
- **Anti-cheat safety:** opening apps and global hotkeys are fine. Nothing
  may hook, inject into or automate input to a running game; kernel
  anti-cheat (e.g. Vanguard) flags that kind of software. Keep the
  satellite a plain user-mode app.
- **Low footprint:** consider replacing rustpotter (which pulls the candle
  ML stack) with openWakeWord or sherpa-onnx keyword spotting on ONNX
  Runtime. Same job, smaller binary. Start at login as a per-user task, not
  a service.

## 9. Cameras and vision

- Use **Frigate** (self-hosted NVR) as the vision backbone instead of
  running models inside Adrian. It already does object detection (including
  `cell phone`), zones, snapshots, and face recognition, cheaply on a Coral,
  OpenVINO or GPU.
- "Where did I leave my phone?" → query Frigate events for the last
  `cell phone` sighting and its zone (for example "living room couch"),
  attach the snapshot, and send a single frame to a vision model only when
  the object detector is unsure. That's one cheap call instead of streaming
  video to a model.
- Faces stay local. Enrollment is opt-in per person, with extra care for
  children. Embeddings and footage follow retention limits. Strict-local
  applies to every camera frame.
- Presence from cameras ("who is in which room") feeds target resolution in
  §4.

## 10. Home Assistant scope

Expand one domain at a time: switches and scenes, then climate, media,
covers, and finally locks and alarm panels. Each domain gets typed
parameters, an allowlist and a minimum assurance level (locks at `voice` or
higher). Physical outcomes keep using the "accepted vs confirmed" pattern.

## 11. Discord companion

Waiting on the full plan. Initial constraints so it fits the rest:

- It's a separate service that talks to the Adrian server over a scoped
  client token, like a satellite but with its own scopes. Memory, identity,
  persona and budgets are therefore shared, not duplicated.
- Voice via Songbird (Rust). Discord delivers one audio stream per speaker,
  which identifies who is talking for free.
- Running 24/7 cheaply: voice-activity gating, local STT, and only invoking
  the LLM when addressed (name, mention, or a configurable "chatty"
  probability). It gets its own monthly budget.
- Discord users can't control the home unless their account is explicitly
  linked to a person and the action's assurance allows it.
- Post a visible notice that the bot transcribes voice, as Discord's rules
  and basic courtesy expect.

## 12. Suggested order

1. **Discord bot** (next, per plan), with the foundation it needs: the
   usage ledger and budgets (§6), the scoped client tokens (§11), and the
   protocol catch-all (F3).
2. **Identity and policy** (§3), which fixes F1, then **rooms and
   announcements with Piper** (§4).
3. **Memory v2** (§5) and **scheduled backups** (F7).
4. **PC satellite v2** (§8).
5. **Frigate integration** (§9), then **more Home Assistant domains** (§10)
   and Wyoming room satellites.

Documentation moves to the GitHub wiki once it is initialized. The wiki is a
separate git repository that can be updated alongside the code.
