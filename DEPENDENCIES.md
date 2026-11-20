# Dependency and license register

State at commit `ac9708d` (PC app 0.17.1), checked 2026-11-20.

- **Versions** in the tables are the requirements written in each crate's `Cargo.toml` (or
  `web/package.json`). `Cargo.lock` is committed and may resolve a newer
  patch release (for example `rustls` 0.23.46, `tray-icon` 0.21.3); only the TLS stack is
  marked "resolved" below. To list what is really built: `cargo tree -p <crate>`.
- **This repo's own license**: the `LICENSE` file is MIT, while `[workspace.package]` in
  `Cargo.toml` declares `license = "Apache-2.0"` for every crate. The two disagree. This is an
  open item for the maintainer; don't change either in a code change.

## Licenses of what is built

Checked from `cargo metadata --locked --offline` on 2026-11-20 (648 resolved packages across
all targets with default features; 689 with `--all-features`, the extra 41 being the optional
`wake-word` stack of `adrian-satellite`, all MIT and/or Apache-2.0) and `web/package-lock.json`:

- Most Rust packages are MIT, Apache-2.0 or dual MIT/Apache-2.0. Also present, among others: ISC
  (`rustls-webpki`, `untrusted`, `libloading`, parts of `aws-lc-rs` / `aws-lc-sys`), BSD-2/3
  (`subtle`, `brotli`, `bindgen`, `encoding_rs` data), Zlib (`foldhash`, `zlib-rs`), Unicode-3.0
  (18 packages), Apache-2.0 WITH LLVM-exception (`target-lexicon`, and as an option in `wasi`,
  `rustix`), Unlicense options (`memchr`, `walkdir`), 0BSD option (`adler2`), CC0 / MIT-0 options
  (`dunce`), BSL-1.0 option (`ryu`), and
  CDLA-Permissive-2.0 (the Mozilla root certificate data in `webpki-roots` /
  `webpki-root-certs`).
- **MPL-2.0** (file-level copyleft): `cssparser`, `cssparser-macros`, `selectors`, `dtoa-short`
  (pulled in by Tauri), `option-ext` (via `directories`), and `symphonia` + three of its
  sub-crates (via `rodio`, old satellite only).
- **No GPL-only package.** `r-efi` and `speech-dispatcher(-sys)` offer LGPL only as one option
  next to MIT/Apache-2.0; `speech-dispatcher` is a Linux-only dependency of `tts`.
- npm (`web/`, devDependencies only, nothing shipped at runtime besides the static build): 102
  MIT, 3 Apache-2.0 (`aria-query`, `axobject-query`, `typescript`), 1 ISC (`picocolors`), 1
  BSD-3-Clause (`source-map-js`).
- `aws-lc-sys` and the bundled SQLite (`libsqlite3-sys`, public domain) compile C code into the
  binaries.

Not shipped in this repo and not redistributed: the WebView2 runtime (part of Windows),
NVIDIA's `nvml.dll` (loaded from the installed driver at call time), any AI model or provider.

## `crates/desktop` (the PC app, `adrian-desktop` 0.17.1)

| Crate | Version | Role |
|---|---|---|
| adrian-pc-control | path | PC actions |
| tauri | `=2.11.6` (feature `tray-icon`) | Window, tray, event loop; pinned because 2.12 needs Rust 1.90 and the workspace MSRV is 1.88. Pulls `wry` 0.55, `tao` 0.35, `webview2-com` 0.38 |
| tauri-build | `=2.6.3` (build) | Embeds `ui/` and generates the capability schemas |
| tokio | 1.53.1 (`rt-multi-thread`, `macros`, `sync`, `time`) | Async runtime for the link |
| tokio-tungstenite | 0.28.0 (`rustls-tls-native-roots`) | WebSocket client to the bot (`ws://` and `wss://`, Windows certificate store) |
| futures-util | 0.3.34 | Stream helpers |
| cpal | 0.16.0 | Mic and speaker streams (WASAPI) |
| global-hotkey | 0.7.0 | Ctrl+Alt+Space push-to-talk |
| directories | 6.0.0 | `%APPDATA%\adrian\desktop\config` |
| rustls | 0.23.45 (resolved 0.23.46; `aws_lc_rs`, `std`, `tls12`, no defaults) | TLS crypto provider (aws-lc-rs), installed once at start |
| ureq | 3.4.2 (`rustls-no-provider`, `rustls-webpki-roots`, `json`) | GitHub API and release downloads for self-update |
| sha2 | 0.10.9 | Checks update downloads against the release `.sha256` |
| base64 | 0.22.1 | `pc_screen` pictures in JSON |
| serde / serde_json | 1 | Settings, alarms file, wire messages |
| windows | 0.58.0 (Windows only) | DPAPI, audio device notifications, process priority and EcoQoS, MMCSS, shell notification state |
| windows-core | 0.58.0 (Windows only) | Needed by name for `#[implement]` COM callbacks |

Dev (Windows only): `windows` 0.58.0 with audio and COM features.

## `crates/pc-control` (`adrian-pc-control` 0.1.0, binary `pcctl`)

| Crate | Version | Role |
|---|---|---|
| serde / serde_json | 1 | Policy, tool arguments |
| windows | 0.58.0 (Windows only) | Every action: windows and processes, audio endpoint/session/meter, WinRT media sessions, power and shutdown, registry, clipboard, GDI and WIC (screenshots, JPEG), DWM, DPI, display config, library loading (NVML) |

Nothing else. No network crate, no async runtime.

## `crates/server` (older household server, `adrian-server` 0.1.0)

| Crate | Version | Role |
|---|---|---|
| adrian-protocol | path | Satellite wire types |
| tokio | 1.53.1 (`full`) | Async runtime |
| axum | 0.8.9 (`ws`) | HTTP router, handlers, WebSocket |
| axum-extra | 0.12.6 (`cookie`) | Cookie helpers |
| axum-server | 0.8.0 (`tls-rustls`) | Built-in HTTPS listener |
| tower / tower-http | 0.5.3 (`util`) / 0.7.1 (`fs`, `trace`) | Middleware, static UI files |
| reqwest | 0.13.5 (`json`, `stream`, `multipart`) | HTTP client: providers, speech-to-text, Home Assistant REST |
| tokio-tungstenite | 0.30.0 | Home Assistant WebSocket |
| rusqlite | 0.40.2 (`bundled`, `backup`) | SQLite (bundled C library), online backup |
| serde / serde_json | 1 | Serialization |
| argon2 | 0.6.0 | Password hashing (Argon2id) |
| rand | 0.10.2 | Tokens (CSPRNG) |
| sha2 | 0.11.0 | Setup and device token hashing |
| chacha20poly1305 | 0.10.1 | Encryption of stored secrets |
| base64 | 0.22.1 | Encoding of sealed secrets |
| jiff | 0.2.37 | Time-zone-aware scheduling (IANA, DST) |
| rustls | 0.23.45 (resolved 0.23.46; `aws_lc_rs`, `std`) | TLS crypto provider |
| tracing / tracing-subscriber | 0.1 / 0.3.23 | Logging |
| futures-util | 0.3.34 | Stream utilities |
| async-stream | 0.3.6 | SSE streams |
| tokio-util | 0.7.19 (`rt`) | Runtime helpers |
| thiserror | 2 | Error types |
| windows-sys | 0.61.2 (Windows only) | DPAPI wrapping of the secrets key |

Dev: `tempfile` 3.27.0, `http-body-util` 0.1.5, `tokio-tungstenite` 0.30.0, `futures-util` 0.3.34.

## `crates/protocol` (`adrian-protocol` 0.1.0)

`serde`, `serde_json`, `thiserror` only.

## `crates/satellite` (old Windows client, `adrian-satellite` 0.1.0)

| Crate | Version | Role |
|---|---|---|
| adrian-protocol | path | Wire types |
| tokio | 1.53.1 (`full`) | Async runtime |
| tokio-tungstenite | 0.28.0 (`rustls-tls-native-roots`) | WebSocket session (voice, alarms) |
| futures-util | 0.3.34 | Stream helpers |
| reqwest | 0.13.5 (`json`) | Pairing, status and media downloads (voice goes over the WebSocket) |
| serde / serde_json | 1 | Config, wire types |
| tracing / tracing-subscriber | 0.1 / 0.3.23 | Logging |
| anyhow | 1.0.100 | Errors |
| cpal | 0.16.0 | Mic capture |
| hound | 3.5.1 | WAV encoding |
| rodio | 0.20.1 | Media playback (alarm tone) |
| tts | 0.26.3 | Speech output (SAPI on Windows) |
| global-hotkey | 0.7.0 | Push-to-talk (Ctrl+Alt+Space) |
| directories | 6.0.0 | `%APPDATA%\adrian\satellite\config` |
| rustls / rustls-native-certs / rustls-pki-types | 0.23.45 (resolved 0.23.46) / 0.8.4 / 1.15.1 | Trusting system roots plus a pinned server CA |
| tray-icon, windows-sys | 0.21.0, 0.61.2 (Windows only) | Tray icon |
| rustpotter, half | 3.0.2, `=2.4.1` (optional feature `wake-word`) | Local wake-word spotting (pulls the candle ML stack) |

Dev: `tempfile` 3.27.0.

## `web/` (UI of the older server)

| Package | Version | Role |
|---|---|---|
| svelte | 5.56.9 | UI framework |
| @sveltejs/kit | 2.70.3 | App framework |
| @sveltejs/adapter-static | 3.0.10 | Static build output |
| @sveltejs/vite-plugin-svelte | 5.1.1 | Vite integration |
| vite | 6.4.3 | Bundler and dev server |
| typescript | 5.9.2 | Type checking |
| svelte-check | 4.7.6 | Component diagnostics |

All are devDependencies; the server serves the static build output.

## Models and external services

- The PC app and `pc-control` call no AI service. Speech recognition, the language model and
  the voice run on the bot server (see the bot's
  [costs.md](https://github.com/Riqqqque/adrian-discord-voice/blob/main/docs/costs.md)).
  The PC app talks only to the bot's satellite WebSocket and, for updates, to the GitHub API
  and GitHub release downloads.
- The older server ships no model or price. Providers are configured at runtime: the OpenAI
  Responses API, or any OpenAI-compatible endpoint (OpenAI, DeepSeek, a local llama.cpp or LM
  Studio server). Their terms apply to their services; nothing is redistributed here.

## Keeping this file current

When you add or bump a dependency: update the table for that crate, keep `windows` at the same
version in `desktop` and `pc-control`, keep Tauri at a version whose MSRV is at most 1.88 (or
raise the workspace `rust-version` on purpose and run `cargo +<new> check --workspace
--all-targets`), and re-run the license check, for example with a short script over
`cargo metadata --locked --all-features --format-version 1`.
