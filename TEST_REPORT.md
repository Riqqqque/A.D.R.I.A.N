# Test report — Gate A

**Commit:** `1529f82`
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
| A-UNIT | `cargo test --workspace` | all pass | 16/16 pass (2 protocol, 11 server lib, 3 HTTP integration) | toolchain output | PASS |
| A-FE-CHECK | `cd web && npm run check` | 0 errors | 0 errors, 30 `a11y_label_has_associated_control` warnings (cosmetic — labels adjacent to inputs, unassociated) | svelte-check output | PASS (with warnings noted) |
| A-FE-BUILD | `cd web && npm run build` | static output emitted | `web/build/index.html` + `_app/` written | build log | PASS |
| A-SAT-BUILD | `cargo check -p adrian-satellite` | compiles | compiles (stub) | toolchain output | PASS |

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
| A-BACKUP | `POST /api/diagnostics/backup` | consistent backup file | `adrian-*.db` written via SQLite Online Backup API | live curl + file on disk | PASS (restore not yet automated → see N-3) |
| A-EGRESS | strict_local + default provider = `api.openai.com`, send chat | refusal | `forbidden: strict-local mode blocks cloud egress — provider 'openai' (api.openai.com) is not a local endpoint`; same request under `hybrid` reaches the provider normally | live curl | PASS |
| A-PROV-TIMEOUT | provider pointed at `127.0.0.1:1`, `POST …/test` | bounded failure | `ok:false, upstream error …` in ~2s, server healthy | live curl | PASS |
| A-TOOL-POLICY | light command path validation | domain+allowlist+action+range checks | enforced in `tools::control_light_inner`/`execute_light` (bad action, unallowlisted entity, brightness>100 all rejected); no unrestricted `call_service` route exists | code review — HA cannot be exercised without an instance | PARTIAL — logic verified by inspection, real HA path NOT RUN |
| A-INJECTION | tool-call args from model are re-validated | policy applied server-side | same validation functions run for model tool calls and UI calls (shared `execute_light`) | code inspection | PASS (design); live injection-vs-provider N/R — needs real model |

## Not run / skipped — needs real external resources

| ID | Item | Why |
|---|---|---|
| N-1 | Real provider streaming chat (OpenAI/DeepSeek) | no API key was provided; adapters are implemented and SSE parsing is written but no real round-trip was executed → **NOT RUN** |
| N-2 | Real Home Assistant connect / entity import / real light command / real `media_player` alarm playback | no HA instance/token/hardware available → **NOT RUN** |
| N-3 | Backup *restore* | backup file verified written; restore procedure not yet automated → **NOT RUN** |
| N-4 | Prompt-injection resistance vs a live model | needs N-1 → **NOT RUN** (server-side policy is model-independent and shared) |
| N-5 | Voice satellite: pairing, mic capture, wake word, PTT, STT, hot-unplug, pre-wake privacy, false-wake metrics | satellite crate is a stub by design; this gate is next → **NOT RUN** |
| N-6 | Browser smoke test of full UI flows | UI serves and all backing APIs verified via curl/integration tests; full click-through not instrumented → **SKIPPED** |
| N-7 | Scheduler restart dedup while an alarm was `ringing` at shutdown | crash-mid-delivery reconciliation implemented (delivery row blocks refire, marks `missed_late`); no crash drill performed → **NOT RUN** |
| N-8 | Rate-limiting lockout | limiter implemented (10/min/IP on login); lockout not exercised → **NOT RUN** |

## Known limitations

- No real provider or Home Assistant credentials were available — every
  claim about those paths is code + unit/integration coverage only.
- Secrets/database/backups are unencrypted at rest (see `docs/SECURITY.md`).
- Satellite binary is a placeholder; no audio capture exists yet.
- svelte-check reports 30 `a11y_label_has_associated_control` warnings.
- Windows Defender intermittently locked build artifacts during development;
  transient `os error 32`/`5` retries were needed (environmental, not a code issue).
