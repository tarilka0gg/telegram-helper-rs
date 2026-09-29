# telegram-helper-rs

Rust rewrite of [Magerko/TelegramHelper](https://github.com/Magerko/TelegramHelper): a personal
AI assistant for a Telegram account. Headless server, analytics web UI on localhost.

- **Userbot** (grammers, MTProto): mirrors every message into SQLite (FTS5), auto-replies while
  you are offline (static or LLM "smart" mode), sends on your behalf.
- **Control bot** (grammers, bot account): commands and free text -> LLM intent router. Anything
  visible to other people is sent only after an inline-button confirmation.
- **Web UI** (axum, `127.0.0.1:8787`): message/LLM analytics, commitments, auto-reply log, events.
  Loopback only, `Host` header checked (DNS-rebinding), refuses non-loopback bind.
- **Zig native lib** (`zig/`, C ABI): SIMD cosine top-k and fuzzy name matching, linked via `tgh-native`.
- Voice transcription was dropped in the rewrite.

## Run

    cp .env.example .env   # fill it in
    cargo run --release -p tgh-server
    # try the dashboard without credentials (synthetic data):
    cargo run -p tgh-server -- --demo

Needs `zig` (0.15+) in `PATH` at build time. Then message the control bot: `/login`
(phone, code with spaces, 2FA), `/key openai sk-...`, `/settings`.

## Layout

| crate | role |
|---|---|
| `tgh-core` | SQLite schema/repo/analytics, Fernet crypto, LLM client (OpenAI/Gemini), intents, HTML sanitizer |
| `tgh-tg` | encrypted DB-backed MTProto session, userbot, control bot, agent, schedulers |
| `tgh-server` | binary: wires everything, axum dashboard |
| `tgh-native` | FFI to `zig/src/native.zig` |

The DB schema is compatible with the Python original's `app.db`. The MTProto session is stored
encrypted in `telegram_sessions.session_string_enc` (Telethon sessions are not portable: `/login` again).

## Tests

    cargo test --workspace && zig test zig/src/native.zig
