# Security model (preview build)

## What is protected

- **Sessions**: random 128-bit ids, `HttpOnly; SameSite=Lax` cookies (plus
  `Secure` when built-in TLS is on), 7-day expiry, revocable. Sessions are rows in SQLite — they survive restarts and
  can be revoked.
- **Mutations** require the per-session CSRF token in `x-adrian-csrf`.
- **Passwords**: Argon2id (PHC format), salted per-user.
- **Setup**: one-use claim token minted on first boot, printed to the server
  console and written to `SETUP_TOKEN.txt` in the data directory.
- **Rate limiting**: login attempts are limited per source IP.
- **Secrets** (provider keys, HA token) are stored server-side only in the
  `secrets` table and never serialized to the frontend. Each value is
  encrypted with ChaCha20-Poly1305 under a 256-bit key, with the secret's
  name bound as associated data. The key lives in `secret.key` in the data
  directory (or `ADRIAN_KEY_FILE`). On Windows that file is wrapped with
  DPAPI for the current user, so a copied data directory or backup cannot
  be decrypted on another account or machine. Backups contain only the
  database, never the key. Secrets written by older builds are encrypted on
  the next start.
- **TLS** (optional): set `ADRIAN_TLS_CERT` and `ADRIAN_TLS_KEY` (PEM) and the
  server serves HTTPS only (rustls). Setting just one of them is refused at
  startup rather than silently falling back to HTTP. Satellites trust the
  system roots plus an optional pinned CA file (`--ca` at pairing).
- **Home Assistant control** is allowlist-only: an entity must be imported,
  be a `light`, be explicitly allowed, and the action must be one of
  `turn_on`/`turn_off`/`toggle` with bounded parameters. There is no generic
  "call any service" API.
- **Memory isolation**: private facts are filtered by owner inside SQL;
  unauthorized rows never leave the database. Voice/room identity is not
  authentication.
- **Media grants**: `/media/{token}` URLs are single-purpose, short-lived
  (10 min) grants so speakers without cookies can fetch alarm audio.
- **Prompt-injection boundary**: tool results, imported entity names, and
  memory text are data, not authority — tool calls are validated against
  allowlists and ownership before execution.

## Honest limitations (this build)

- **Only the secrets table is encrypted.** Memory facts, chat history,
  alarms, the action ledger, session ids and the rest of the database (and
  its backups) are stored in plaintext. Anyone with file access to the data
  directory can read them. Do not run this on a shared machine you don't
  trust.
- Outside Windows the key file holds the raw key (mode 0600). If it sits in
  the data directory it only protects backups and copies of the database
  alone; point `ADRIAN_KEY_FILE` somewhere else to keep the two apart.
- Restoring a backup on another machine or Windows account, or losing the
  key file, makes stored secrets unreadable: re-enter provider keys and the
  Home Assistant token. Nothing else is affected.
- TLS is off by default and bind is loopback. For LAN access set
  `ADRIAN_BIND=0.0.0.0:8787` and either enable built-in TLS or put a
  TLS-terminating reverse proxy in front.
- Home Assistant playback "success" means the service call was accepted;
  audibility is not confirmed by this build.
- Browser audio endpoints require the tab to stay open and logged in.
- `strict_local` privacy mode blocks cloud provider calls at the policy
  layer; it does not firewall the host.

## Reporting

Found a problem? Open an issue describing reproduction and impact. Do not
file public issues containing tokens or dumps of the secrets table.
