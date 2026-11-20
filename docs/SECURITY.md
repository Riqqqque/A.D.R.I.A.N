# Security model

This repo holds two stacks with separate security models (see
[architecture.md](architecture.md)):

1. **The PC app** (`crates/desktop`) and **PC control** (`crates/pc-control`): what people
   install. Its server side is the bot repo, whose own security notes are in
   [adrian-discord-voice docs/security.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/security.md).
2. **The older Rust household server** (`crates/server`, `crates/satellite`, `web/`): a
   preview build that is not deployed. Its model is described in the second half of this page.

## The PC app and PC control

### Trust boundaries

| Party | Trusted for | Not trusted for |
|---|---|---|
| The bot server the PC is paired with | Deciding what was said and what to do, within this PC's policy | Changing the policy; running actions or setting alarms before `welcome`; picking another person's PC |
| The language model (on the bot) | Choosing a tool and its arguments | Confirming power actions; tool results, clipboard text and screen pictures are data, not instructions |
| Anyone talking near the PC | Nothing beyond the PC owner's rights | See "Known risks" below: there is no voice identification |
| The PC's own settings sheet | The policy, the token, the server address | |

### What protects the PC

- **Local policy is the final word.** Every action goes through `Pc::run`, which checks the
  PC's own `Policy` (`crates/pc-control/src/policy.rs`) before doing anything. The policy
  lives only in the PC's settings file and the server has no message that reads or changes
  it. Unknown tool names are refused. Every switch is on by default; turn off what you don't
  want in "What she may do on this PC". Details: [pc-control.md](pc-control.md#the-policy).
- **Typed actions only.** There is no shell, no keystroke or mouse injection, no killing of
  processes (close is `WM_CLOSE`), and nothing is started except installed apps and games and
  the shortcuts the PC's owner added. `.ps1` shortcuts run with `-ExecutionPolicy Bypass`, so
  only add scripts you trust.
- **Games are left alone.** Windows of games and of programs the PC can't identify are never
  closed, focused, minimized, moved or pictured, and other processes are opened only with
  `PROCESS_QUERY_LIMITED_INFORMATION`, so kernel anti-cheat has nothing to flag. Exceptions
  are listed in [pc-control.md](pc-control.md#known-gaps).
- **Power actions need a person's yes.** Sleep, restart and shutdown are refused on the PC
  without `confirmed: true`. The bot strips any `confirmed` the model sends and sets it only
  after the person's own next-turn yes to a fixed question, within 20 seconds. Restart and
  shutdown wait 30 seconds and a plain "cancel" stops them (`shutdown.exe /a`).
- **Only public web pages open.** `pc_open url` accepts only `http`/`https`, at most 300
  bytes, a plain host name (no raw IP addresses, no `localhost`, `.local`, `.lan`, `.home`,
  `.internal`, `.intranet`, `.corp`, `.arpa`), and every address the name resolves to must be
  public (no loopback, private, link-local, CGNAT). The bot runs the same name rules without
  the DNS lookup.
- **Clipboard and screen.** Read only when asked. Clipboard content marked by a password
  manager (the standard "exclude from monitoring" and "no history / no cloud" formats) is
  refused without reading it; text is capped at 8000 characters. Screen pictures are refused
  for games, unidentifiable programs, full-screen apps in front, and this app's own window;
  the reply never includes the window title. The bot keeps both out of its logs and gives a
  picture to exactly one model call.
- **Nothing runs before `welcome`.** Until the server accepts the token (`welcome`), the PC
  sends only `hello`, `ping` and refusals, runs no `action` ("Not connected yet."), since
  whatever answers at the configured address could send one, and ignores `alarms`. It does
  play audio and show status and transcript text from whatever answers at the configured
  address before any token check, so point it only at your own server.
- **Nothing stale.** Anything typed, pressed or recorded while offline is dropped, not sent
  later, so an old "lock the PC" can't run minutes after it was said. Push-to-talk never
  survives a lost link. From the next release the PC also drops speech that waited more than
  1.5 s for speakers that were gone, so a late answer isn't played long after it was said.
- **Audio leaves only while someone talks.** The mic is always open, but frames are sent only
  while the local loudness gate is open (plus 300 ms from just before). Offline audio is
  dropped and the queue is bounded at 30 s.
- **Token at rest.** The `sat_` pairing token is encrypted with Windows DPAPI for the current
  Windows user (`token_protected` in `%APPDATA%\adrian\desktop\config\config.json`) and never
  shown in the window. On the bot server only its SHA-256 is stored, compared in constant time.
- **Window.** The web view's CSP allows only the app's own scripts and styles, no remote
  content and no inline scripts. No window exists while the app is in the tray.
- **Updates.** Only from this project's GitHub release download URLs; the exe must match the
  release's `.sha256`, must start and print exactly the release tag under `--self-check`, is
  re-hashed just before the swap, installs only while the app is idle in the tray with no
  full-screen app in front, and the previous exe is kept until the new one has been welcomed
  by the server and run for 3 minutes. Auto-update can be turned off in settings. Details:
  [desktop.md](desktop.md#self-update).
- **NVIDIA library** for health checks is loaded only from System32 (or NVIDIA's NVSMI folder
  under Program Files), never from the current folder or PATH, and unloaded after each call.

### What the bot server does for PC links

From the bot repo (`src/satellite/server.ts`, `devices.ts`, `tools.ts`); see its
[satellite.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md):

- `hello` must arrive within 5 s. Five failed attempts (no hello, a bad first frame, a wrong
  token) from one address lock that address out for 10 minutes; a correct token still gets
  in, so PCs behind one shared address aren't locked out by someone else.
- Frames are capped at 256 KiB before `welcome` and for everything except `action_result`
  (2 MiB + 64 KiB). One live connection per paired device; a reconnect replaces the old one.
- A PC tool only ever reaches the asker's own connected PC. No tool argument names a PC or a
  person.
- Transcript logging of PC requests (`satellite.logTranscripts`) is off by default. When it is
  on, logs contain spoken requests, her answers, PC action arguments and the first 200
  characters of each action's result (for example window titles from `pc_apps running`), but
  never clipboard text or screen pictures.

### Known risks and limits

- **Anyone near a paired PC has its owner's rights** (roadmap F1). There is no voice
  identification: whoever talks near the PC, or whatever audio reaches its mic, acts as the
  PC's owner, including any owner-only powers that person has on the bot. The per-PC policy
  and the spoken yes for power actions are the stopgap. Turn off what you don't need.
- **A voice call heard through the PC's speakers** is a way in: people in the call speak
  through the PC's mic. From the next release the app reports whether its output is
  headphones, speakers or unknown (`output`), and the bot then takes only push-to-talk and
  typing at a PC on speakers or unknown while its owner is in any voice call the bot can see
  (the rule and its edges are in the bot's
  [satellite.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/satellite.md)).
  The setting "These speakers are headphones" is trusted as given: ticking it for real
  speakers turns that guard off for the PC. Calls the bot can't see (servers it isn't in,
  direct-message calls, other apps, a video) are not covered. Older apps never send
  `output`, so they are always guarded in a call.
- **link.log** (next release) is a local file in the settings folder. It blanks the server's
  host name and the token, but a TLS certificate-name error can still include the other
  names and addresses on the server's certificate. Read it before sharing it.
- **`ws://` is plain text.** The bot's satellite endpoint speaks plain WebSocket; the token
  (in `hello`) and audio travel unencrypted unless you use a private network, a VPN, or a TLS
  reverse proxy in front of the bot and a `wss://` (or `https://`) server address in the app.
- **Pairing tokens don't expire.** To rotate one, remove the device on the bot
  (`adrian satellite remove`) and pair again. Removing it doesn't drop a connection that is
  already open until the bot restarts or the PC reconnects.
- **DPAPI protects against other Windows users and copied files, not against programs running
  as the same user.** Anything running in your Windows session can decrypt the token.
- **The release exe is not code-signed.** Windows SmartScreen warns on first run.
- **A few PC actions don't check for games**: master and per-app volume, `screen_off` (a
  broadcast message), `show_desktop` (the shell's minimize-all), and media controls. See
  [pc-control.md](pc-control.md#known-gaps).
- **`pc_apps running` sends window titles** (first 60 characters) to the model; titles can
  contain document, page or chat names.
- **A corrupt settings file loads as defaults** (unpaired, everything allowed), and the next
  Save overwrites it.

## The older Rust household server

This applies to `crates/server`, its web UI and `crates/satellite`. It is a preview build, not
what the PC app uses.

### What is protected

- **Sessions**: random 128-bit ids, `HttpOnly; SameSite=Lax` cookies (plus `Secure` when
  built-in TLS is on, or when the trusted proxy reports https), 7-day expiry. Sessions are rows
  in SQLite: they survive restarts and can be revoked.
- **Mutations** require the per-session CSRF token in `x-adrian-csrf`.
- **Passwords**: Argon2id (PHC format), salted per user. At least 8 characters.
- **Setup**: a one-use claim token minted on first boot, printed to the server console and
  written to `SETUP_TOKEN.txt` in the data directory; only its SHA-256 is stored.
- **Roles and capabilities**: `owner`, `member`, `guest`. Owner and member hold every
  capability (chat, lights, alarms, speakers); a guest holds only chat; any other role holds
  nothing. Many configuration routes additionally require the owner.
- **Rate limiting** (in memory): login 10 per minute per address, setup claim 8 per minute
  per address, satellite pairing 10 per minute per address, chat 30 messages per minute per
  user. One model turn per user at a time.
- **Secrets** (provider keys, the Home Assistant token) are stored server-side only in the
  `secrets` table and never sent to the frontend. Each value is encrypted with
  ChaCha20-Poly1305 under a 256-bit key, with the secret's name bound as associated data
  (`enc:v1:` prefix). The key lives in `secret.key` in the data directory (or
  `ADRIAN_KEY_FILE`). On Windows that file is wrapped with DPAPI for the current user, so a
  copied data directory or backup cannot be decrypted on another account or machine. Backups
  contain only the database, never the key. Secrets written by older builds are encrypted on
  the next start.
- **TLS** (optional): set `ADRIAN_TLS_CERT` and `ADRIAN_TLS_KEY` (PEM) and the server serves
  HTTPS only (rustls). Setting just one of them is refused at startup rather than silently
  falling back to HTTP. Satellites trust the system roots plus an optional pinned CA file
  (`--ca` at pairing).
- **Home Assistant control** is allowlist-only: an entity must be imported, be a `light`, be
  explicitly allowed by the owner, and the action must be one of `turn_on`/`turn_off`/`toggle`
  with brightness 1 to 100. There is no generic "call any service" API. Non-owners only see
  allowed lights.
- **The model never authorizes.** Every tool checks capabilities and database allowlists in
  code. Household-wide memory is written only when the person's own message says household,
  family, everyone or everybody, and never by a guest. The model can't pick an alarm's volume.
- **Prompt data is fenced.** Facts and light names go into a `<data>` block after the rules,
  with `<` escaped so stored text can't close the block or add rules.
- **Memory isolation**: private facts are filtered by owner inside SQL; unauthorized rows
  never leave the database. Voice or room identity is not authentication.
- **Media grants**: `/media/{token}` URLs are single-purpose, short-lived (10 min) grants so
  speakers without cookies can fetch alarm audio.
- **Revocation**: revoking a satellite disconnects every live socket on its token and disables
  its alarm endpoints; a revoked device or disabled owner gets no turn, even mid-transcription.
- **Prompt-injection boundary**: tool results, imported entity names and memory text are data,
  not authority; tool calls are validated against allowlists and ownership before execution.
- **Web UI headers**: CSP with the hash of the inline boot script, `x-content-type-options`,
  `x-frame-options: DENY`, `referrer-policy: same-origin`.

### Honest limitations

- **Only the secrets table is encrypted.** Memory facts, chat history, alarms, the action
  ledger, session ids and the rest of the database (and its backups) are stored in plaintext.
  Anyone with file access to the data directory can read them. Do not run this on a shared
  machine you don't trust.
- Outside Windows the key file holds the raw key (mode 0600). If it sits in the data directory
  it only protects backups and copies of the database alone; point `ADRIAN_KEY_FILE`
  somewhere else to keep the two apart.
- Restoring a backup on another machine or Windows account, or losing the key file, makes
  stored secrets unreadable: re-enter provider keys and the Home Assistant token. Nothing else
  is affected.
- TLS is off by default and the bind is loopback (`127.0.0.1:8787`). For LAN access set
  `ADRIAN_BIND=0.0.0.0:8787` and either enable built-in TLS or put a TLS-terminating reverse
  proxy in front. An unparsable `ADRIAN_BIND` silently falls back to the loopback default.
- Behind a proxy, set `ADRIAN_TRUSTED_PROXY` to its address; without it the session cookie is
  not marked `Secure` and all clients share one login rate limit. Forwarded headers are
  ignored from every other address, and the proxy must set `X-Forwarded-Proto` itself and
  append the client to `X-Forwarded-For`, or a client could forge them.
- **A satellite acts as the user who paired it, with that user's full role.** Only the owner
  can mint pairing codes, so every satellite acts as the owner (roadmap F1).
- There is no API to create member or guest users, change a password or revoke someone else's
  sessions; in practice the server has one owner.
- `GET /api/diagnostics` is open to any signed-in user and shows provider labels, models and
  Home Assistant status.
- `SETUP_TOKEN.txt` is not deleted after the claim (the token can't be reused).
- Home Assistant playback "success" means the service call was accepted; audibility is not
  confirmed by this build.
- Browser audio endpoints require the tab to stay open and signed in.
- `strict_local` privacy mode blocks cloud provider calls at the policy layer; it does not
  firewall the host.

## Reporting

Found a problem? Open an issue describing how to reproduce it and its impact. Do not post
tokens, keys, pairing tokens, settings files, logs with transcripts, or dumps of the secrets
table in a public issue.
