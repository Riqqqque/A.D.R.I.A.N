# Dependency & license register

All Rust dependencies are MIT, Apache-2.0, MIT/Apache-2.0 dual, or BSD-family
licensed (per crates.io metadata). All npm dependencies are MIT or
Apache-2.0 licensed. No GPL components are used in this build.

## Rust — server (`crates/server`)

| Crate | Version | Role |
|---|---|---|
| tokio | 1.53 | async runtime |
| axum | 0.8 | HTTP router/handlers |
| tower / tower-http | 0.5 / 0.7 | middleware, static file serving |
| axum-extra | 0.12 | cookie extraction helpers |
| reqwest | 0.13 | HTTP client (providers, Home Assistant) |
| tokio-tungstenite | 0.30 | Home Assistant WebSocket |
| rusqlite | 0.40 (`bundled`, `backup`) | SQLite, bundled C library, online backup |
| serde / serde_json | 1.0 | serialization |
| argon2 | 0.6 | password hashing (Argon2id) |
| rand | 0.10 | token generation (CSPRNG) |
| sha2 | 0.11 | setup-token hashing |
| jiff | 0.2 | timezone-aware scheduling (IANA tz, DST) |
| tracing / tracing-subscriber | 0.1 / 0.3 | logging |
| futures-util | 0.3 | stream utilities |
| async-stream | 0.3 | SSE stream construction |
| tokio-util | 0.7 | runtime helpers |
| thiserror | 2.0 | error types |

Dev: `tempfile`, `http-body-util` (tests only).

Notable transitive: `aws-lc-rs` (via rustls in reqwest) — TLS crypto;
`libsqlite3-sys` (bundled SQLite, public domain + blessing).

## Rust — protocol (`crates/protocol`)

serde, serde_json, thiserror only.

## Rust — satellite (`crates/satellite`)

| Package | Version | Role |
|---|---|---|
| tokio | 1.53 | async runtime |
| reqwest | 0.13 | utterance transport to the server |
| serde / serde_json | 1 | config + wire types |
| tracing / tracing-subscriber | 0.1 / 0.3 | logging |
| anyhow | 1.0 | error handling |
| cpal | 0.16 | microphone capture |
| hound | 3.5 | WAV encoding |
| tts | 0.26 | local speech synthesis (SAPI on Windows) |
| global-hotkey | 0.7 | push-to-talk key (Ctrl+Alt+Space) |
| directories | 6.0 | per-user config dir resolution |

## npm — frontend (`web/`)

| Package | Version | Role |
|---|---|---|
| svelte | 5.56.9 | UI framework |
| @sveltejs/kit | 2.70.3 | app framework |
| @sveltejs/adapter-static | 3.0.10 | static build output |
| @sveltejs/vite-plugin-svelte | 5.1.1 | vite integration |
| vite | 6.4.3 | bundler/dev server |
| typescript | 5.9.2 | typechecking |
| svelte-check | 4.7.6 | component diagnostics |

All devDependencies — nothing is required at runtime; the server serves the
static build output.

## Models / external services

No model artifacts are vendored. AI providers are configured at runtime
(OpenAI Responses API, DeepSeek or any OpenAI-compatible endpoint, or a
local server such as llama.cpp/LM Studio). Their respective terms apply to
their services; they are not redistributed here.
