# Security model (preview build)

## What is protected

- **Sessions**: random 128-bit ids, `HttpOnly; SameSite=Lax` cookies, 7-day
  expiry, revocable. Sessions are rows in SQLite — they survive restarts and
  can be revoked.
- **Mutations** require the per-session CSRF token in `x-adrian-csrf`.
- **Passwords**: Argon2id (PHC format), salted per-user.
- **Setup**: one-use claim token minted on first boot, printed to the server
  console and written to `SETUP_TOKEN.txt` in the data directory.
- **Rate limiting**: login attempts are limited per source IP.
- **Secrets** (provider keys, HA token) are stored server-side only in the
  `secrets` table and never serialized to the frontend.
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

- **The database, indexes, and backups are stored unencrypted.** Anyone with
  file access to the data directory can read everything, including secrets.
  Do not run this on a shared machine you don't trust. Encrypted-at-rest
  storage is a roadmap item, not a current claim.
- No TLS is built in — bind is loopback by default. For LAN access set
  `ADRIAN_BIND=0.0.0.0:8787` and prefer putting a TLS-terminating reverse
  proxy in front.
- Home Assistant playback "success" means the service call was accepted;
  audibility is not confirmed by this build.
- Browser audio endpoints require the tab to stay open and logged in.
- `strict_local` privacy mode blocks cloud provider calls at the policy
  layer; it does not firewall the host.

## Reporting

Found a problem? Open an issue describing reproduction and impact. Do not
file public issues containing tokens or dumps of the secrets table.
