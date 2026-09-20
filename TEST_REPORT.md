# Test report — Gate A + early Gate B

**Commits:** `1529f82` … `de88194`+ (see `git log`; this file is amended into the satellite commit)
**Date:** 2026-09-17
**OS/Hardware:** Windows 11 (x86_64), MSVC Build Tools 2022 (14.44), local dev machine
**Toolchain:** rustc/cargo 1.94.1 stable-msvc · Node 24.12.0 · bundled SQLite 3.53.2
**Evidence types:** `UNIT/FIXTURE` = automated unit/integration test · `LIVE SERVER` = real running server + real SQLite over real HTTP, no mocks, no external service · `REAL PROVIDER`/`REAL HARDWARE` = real external dependency · `NOT RUN`/`SKIPPED` = honest non-execution.

Evidence for `LIVE SERVER` rows: terminal transcripts of curl invocations against
`http://127.0.0.1:8787` with `ADRIAN_DATA_DIR=%TEMP%\adrian-test-data` (not committed;
reproducible with the commands shown).

## Automated

| ID | Procedure | Expected | Observed | Evidence | Result |
|---|---|---|---|---|---|
| A-FMT | `cargo fmt --all --check` | clean | clean | toolchain output | PASS |
| A-CLIPPY | `cargo clippy --workspace` | 0 warnings | 0 warnings | toolchain output | PASS |
| A-UNIT | `cargo test --workspace` | all pass | 28/28 pass (2 protocol, 15 server lib, 7 HTTP integration, 4 WS session) | toolchain output | PASS |
| A-FE-CHECK | `cd web && npm run check` | 0 errors, 0 warnings | 0 errors, 0 warnings (a11y warnings from previous report all fixed — controls wrapped in labels) | svelte-check output | PASS |
| A-FE-BUILD | `cd web && npm run build` | static output emitted | `web/build/index.html` + `_app/` written | build log | PASS |
| A-SAT-BUILD | `cargo check -p adrian-satellite` | compiles | compiles (real client: cpal mic, global-hotkey PTT, reqwest transport, tts output) | toolchain output | PASS |

## Gate A behavior

| ID | Procedure | Expected | Observed | Evidence | Result |
|---|---|---|---|---|---|
| A-SETUP-1 | `POST /api/setup/claim` with console-minted token | owner created, session cookie, CSRF issued | 200 + `adrian_session` cookie + csrf; second claim with same token → 403 `setup token invalid or already used` | integration test `setup_is_one_use_and_sessions_work` + live curl | PASS |
| A-AUTH-1 | `GET /api/memory` with no cookie; mutation with bad/missing CSRF | 401 / 403 | 401 unauthenticated; 403 bad or missing `x-adrian-csrf` | integration test | PASS |
| A-PERSIST-1 | Write fact, restart server, `GET /api/memory?q=…` | fact + session survive | fact returned, same session cookie still valid (verified live and in `data_survives_restart`) | integration test + live restart | PASS |
| A-MEM-1 | Add/list/FTS-search/edit/delete fact | versioned CRUD, FTS reflects edits | all pass; `v2` after edit; deleted fact gone from list + search | unit test `add_search_update_delete` | PASS |
| A-MEM-ISO | User B reads/edits/deletes user A's private fact; B's list/search | denied everywhere | `get`→none, `update`/`delete`→err, list+FTS exclude it | unit test `private_facts_are_isolated_per_owner` | PASS |
| A-ALARM-1 | `POST /api/alarms` with no audio endpoint | honest refusal, nothing committed | 412 `not_configured`; alarm list empty | integration test + live curl | PASS |
| A-ALARM-2 | Register browser endpoint, create 1-min timer, listen on SSE channel | play command at due time, durable record | `persisted:true` + resolved local time at create; `cmd/play` SSE frame arrived at due time; alarm → `done`; delivery recorded `failed — no ack` because the curl listener didn't ack (honest outcome) | live server transcript | PASS |
| A-ALARM-3 | `POST /api/endpoints/{id}/test` then `POST /api/audio/ack` | ack completes the test call | test returned `{"ok":true,"detail":"played"}` after ack; `/media/{token}` served a real PCM WAV | live server transcript | PASS |
| A-ALARM-DST | resolve `2026-03-08 02:30` America/New_York (nonexistent) | springs forward, no crash | resolved to `03:30` EDT | unit test `dst_gap_springs_forward` | PASS |
| A-ALARM-WDAY | `09:00 weekdays` asked Friday evening | next Monday | resolved `2026-09-21` (Monday) | unit test `weekdays_skip_weekend` | PASS |
| A-DIAG | `GET /api/diagnostics` | honest status + warnings | `journal_mode=wal`, `synchronous=2` (FULL), counts, warnings listing every missing integration | live curl | PASS |
| A-BACKUP | `POST /api/diagnostics/backup` | consistent backup file | `adrian-*.db` written via SQLite Online Backup API | live curl + file on disk | PASS |
| A-EGRESS | strict_local + default provider = `api.openai.com`, send chat | refusal | `forbidden: strict-local mode blocks cloud egress — provider 'openai' (api.openai.com) is not a local endpoint`; same request under `hybrid` reaches the provider normally | live curl | PASS |
| A-PROV-TIMEOUT | provider pointed at `127.0.0.1:1`, `POST …/test` | bounded failure | `ok:false, upstream error …` in ~2s, server healthy | live curl | PASS |
| A-TOOL-POLICY | light command path validation | domain+allowlist+action+range checks | enforced in `tools::control_light_inner`/`execute_light` (bad action, unallowlisted entity, brightness>100 all rejected); no unrestricted `call_service` route exists | code review — HA cannot be exercised without an instance | PARTIAL — logic verified by inspection, real HA path NOT RUN |
| A-INJECTION | tool-call args from model are re-validated | policy applied server-side | same validation functions run for model tool calls and UI calls (shared `execute_light`) | code inspection | PASS (design); live injection-vs-provider N/R — needs real model |
| A-CHAT-HIST | `GET /api/chat/conversations` + `/{id}` | owner-scoped list + history | conversations list with last message; history returns ordered messages; cross-user id → 404 | live curl + code | PASS |
| A-RECONCILE | crash while recurring alarm `ringing` | rescheduled, not killed | interrupted delivery marked failed; one-shot → `done`; recurring → next occurrence persisted; no duplicate delivery row | integration test | PASS |
| A-RATELIMIT | >10 login attempts/min/IP | 429 lockout | limiter trips at threshold, recovers after window | unit test | PASS |
| B-PAIR | `POST /api/satellites/pairing-code` → `POST /api/satellite/pair` | one-use code → device token | 6-digit code minted (5-min expiry); satellite redeemed → `sat_*` token + id; **code reuse → 403**; satellite CLI `pair` + `status` verified | integration test `satellite_pair_status_and_revoke` + live server | PASS |
| B-AUTH | utterance/status with wrong or revoked token | 401 | bogus token → 401; after `DELETE /api/satellites/{id}` the token → 401 | integration test + live curl | PASS |
| B-UTTER | WAV POST → transcribe → assistant turn → reply text | full pipeline | mock local STT (`/v1/audio/transcriptions`) returned "what time is it"; reply "It's Friday … UTC"; conversation persisted + `conversation_id` continuity honored | live server + mock STT | PASS (real whisper STT N/R — see N-9) |
| B-STRICT | strict_local + cloud STT base | refusal before egress | cloud base URL → 403 `strict-local … needs a local STT endpoint` | code path shared with A-EGRESS | PASS (same check) |
| B-PAIR-RATE | >10 `POST /api/satellite/pair` attempts/min/IP | 429 lockout | 11th attempt → 429; pairing now shares the login/setup limiter | integration test `pair_is_rate_limited_and_one_use_is_atomic` | PASS |
| B-PAIR-RACE | 6 concurrent redemptions of one pairing code | exactly one token issued | 1×200 + 5×403; the burn is an atomic `UPDATE … WHERE used=0 … RETURNING` | same test | PASS |
| B-WAV-LIMIT | >2MB WAV body to `/api/satellite/utterance` | reaches handler (axum 2MB default lifted) | 3MB body → 412 (bad wav), not 413 | same test | PASS |
| B-WS-AUTH | `GET /api/satellite/session` with no/bogus token vs valid token | 401 vs upgrade | no/bogus → HTTP error during handshake; valid → upgrades | ws_session test `session_requires_device_token` | PASS |
| B-WS-PROTO | versioned envelopes over the WS: bad version, AudioSessionStart+binary+End, idempotency replay | nack / capturing+processing acks / duplicate ignored | all observed on a real TCP+WS round trip | ws_session test `session_protocol_envelopes` | PASS |
| B-SAT-ALARM | satellite endpoint + due alarm, satellite connected | `Speak` pushed over WS, ack → delivery `played` | media_url envelope arrived; ack resolved the armed waiter; row `played` | ws_session test `satellite_alarm_is_delivered_over_ws` | PASS |
| B-SAT-ALARM-OFF | same with satellite offline | honest failure | delivery row `failed`, detail "not connected" | ws_session test `satellite_alarm_offline_is_failed_not_silent` | PASS |
| B-RESTORE | backup → mutate → `POST /api/diagnostics/restore` | original state returns; queries keep working | marker value restored; post-restore `/api/diagnostics` healthy; traversal + missing file rejected | integration test `backup_restore_roundtrip` | PASS |

## Not run / skipped — needs real external resources

| ID | Item | Why |
|---|---|---|
| N-1 | Real provider streaming chat (OpenAI/DeepSeek) | no API key was provided; adapters are implemented and SSE parsing is written but no real round-trip was executed → **NOT RUN** |
| N-2 | Real Home Assistant connect / entity import / real light command / real `media_player` alarm playback | no HA instance/token/hardware available → **NOT RUN** |
| N-3 | Backup *restore* | implemented + tested → **SUPERSEDED by B-RESTORE** |
| N-4 | Prompt-injection resistance vs a live model | needs N-1 → **NOT RUN** (server-side policy is model-independent and shared) |
| N-5 | Satellite wake word, hot-unplug behavior, pre-wake privacy, false-wake metrics | wake word implemented (rustpotter, opt-in `wake-word` feature + `.rpw` model config; 2s local rolling buffer, silence endpointing) and hot-unplug re-init implemented — but neither exercised against real audio hardware/model → **NOT RUN** |
| N-9 | Real STT round-trip (OpenAI whisper / local whisper server) and real mic capture + TTS playback | pipeline verified against a mock local `/audio/transcriptions` endpoint; no real STT service or mic session exercised → **NOT RUN** |
| N-10 | Satellite streaming session protocol (WS), tray UI, satellite-as-alarm-endpoint | WS protocol + alarm endpoint implemented and tested (B-WS-*, B-SAT-ALARM); tray icon implemented (Windows pump thread) but not visually exercised → **SUPERSEDED** (tray visual check pending) |
| N-6 | Browser smoke test of full UI flows | UI serves and all backing APIs verified via curl/integration tests; full click-through not instrumented → **SKIPPED** |
| N-7 | Scheduler restart dedup while an alarm was `ringing` at shutdown | covered by the reconcile integration test (delivery row blocks refire; recurring reschedules); no manual crash drill → **SUPERSEDED by A-RECONCILE** |
| N-8 | Rate-limiting lockout | covered by unit test → **SUPERSEDED by A-RATELIMIT** |

## Known limitations

- No real provider or Home Assistant credentials were available — every
  claim about those paths is code + unit/integration coverage only.
- Secrets/database/backups are unencrypted at rest (see `docs/SECURITY.md`).
- Satellite runs a persistent WS session (PTT + optional local wake word,
  tray icon, alarm playback). Mic capture, TTS output, wake-word spotting and
  the tray icon are compiled but not exercised on real hardware in this
  report — the wake-word path additionally needs a user-supplied `.rpw`
  model.
- svelte-check reports 0 warnings (previous a11y findings fixed).
- Windows Defender intermittently locked build artifacts during development;
  transient `os error 32`/`5` retries were needed (environmental, not a code issue).
