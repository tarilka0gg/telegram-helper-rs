# telegram-helper-rs

Rust rewrite of [Magerko/TelegramHelper](https://github.com/Magerko/TelegramHelper): a personal AI assistant
for a Telegram account. **Headless** server, analytics web UI on localhost only.

| part | what it does |
|---|---|
| **Userbot** (grammers / MTProto) | mirrors messages into SQLite + FTS5, auto-replies while you are offline (static or LLM "smart"), sends on your behalf |
| **Control bot** (grammers, bot account) | commands and free text → LLM intent router; anything visible to other people needs an inline-button confirmation |
| **Web UI** (axum, `127.0.0.1:8787`) | analytics dashboard, `/chats` (switch mirroring / news sources per chat, avatars), `/login` (QR sign-in) |
| **Zig lib** (`zig/`, C ABI) | SIMD cosine top-k and fuzzy name matching, linked through `tgh-native` |

Voice transcription was dropped in the rewrite.

## Run

```
cp .env.example .env        # BOT_TOKEN, OWNER_TELEGRAM_ID, TG_API_ID, TG_API_HASH, ENCRYPTION_KEY
cargo run --release -p tgh-server
cargo run -p tgh-server -- --demo      # dashboard with synthetic data, no credentials needed
```

`zig` 0.16+ (tested with 0.16.0) must be in `PATH` at build time. Sign in to the userbot from the control bot:
`/login` (phone, code **with spaces**, 2FA) or `/qr` (scan the QR shown on `http://127.0.0.1:8787/login`).
Then `/key <provider> <key>` (or the CLI below).

### CLI

| command | purpose |
|---|---|
| `tgh-server key <gemini\|groq\|zai\|openai>` | store a provider key read from **stdin** (validated, stored encrypted) |
| `tgh-server import-keys <file.env>` | validate and store `GEMINI_API_KEY`, `GROQ_API_KEY`, `ZAI_API_KEY`, `OPENAI_API_KEY` from another project's `.env` |
| `tgh-server ask "question"` | one LLM call through the configured chain (smoke test) |
| `POST /api/selftest` | live end-to-end check inside the running server (see *Testing*) |

Run as a service: `deploy/tgh-server.service` (systemd user unit).

## Bot commands

`/login` `/qr` `/logout` `/status` `/sync` · `/key` `/settings` `/set key value` · `/chat Name` `/catchup Name` `/send instruction`
`/search text` `/todos` `/digest [on|off|at HH:MM]` `/news [topic]` `/topics` `/classify` `/sources Name`.
Free text works too: “write Olya that the call is at 8”, “remind me tomorrow 18:00 to call mom”, “what's new in the Starfield channel”.

## Behaviour worth knowing

- **LLM chain**: the chosen provider first, then every other provider that has a key (Gemini → Groq → Z.ai → OpenAI).
  A spent *daily* quota skips that model for 30 min; other errors fall through to the next provider. Transient 429/5xx are retried.
- **Mirror**: per chat switch on `/chats`. Archived chats are neither mirrored nor shown while `ignore_archived` is on (default).
  New chats the classifier files under *entertainment* start with mirror off. Explicit questions about a chat fetch fresh messages
  from Telegram first (and never store them for non-mirrored chats).
- **News**: `/news` pulls recent posts from the source channels, uses the last 24 h (or the latest post per source if there is nothing new)
  and never repeats a post it already delivered.
- **First run**: chats and channels are sorted by context with the LLM (names + up to 3 sample messages go to the provider); manual choices are never overwritten.
- **Schedulers** (digest, news, reminders, hourly sync) fire once per day inside a 90-minute window and survive restarts.

## Security model

- The web UI has **no authentication**, so it binds to loopback only (refuses anything else unless `ALLOW_REMOTE_UI=1`), rejects
  requests whose `Host` is not loopback (DNS rebinding) and requires a custom header + local `Origin` for every state-changing request (CSRF).
- Secrets (MTProto sessions of the userbot *and* the bot, provider keys) live in SQLite **encrypted with Fernet** (`ENCRYPTION_KEY`).
  Messages containing keys, codes or passwords are deleted from the chat right after use.
- Only `OWNER_TELEGRAM_ID` is served; other senders get no reply at all.
- LLM output is sanitized to Telegram's HTML subset (no `javascript:` links, tags whitelisted) before it is sent.
- Smart auto-reply feeds the stranger's message to the model: replies are capped at 600 chars, but treat it as untrusted-input → owner's voice.

## Layout

| crate | role |
|---|---|
| `tgh-core` | SQLite schema/repo/analytics, Fernet, LLM chain, intents, HTML sanitizer |
| `tgh-tg` | DB-backed MTProto sessions, login (code / QR), userbot, control bot, agent, schedulers, classifier, news, self-test |
| `tgh-server` | binary: wiring + axum dashboard |
| `tgh-native` | FFI to `zig/src/native.zig` |

The schema is compatible with the Python original's `app.db` (its FTS objects are detected and reused; new columns are added by migration v2).
Telethon sessions are not portable: sign in again.

## Testing

```
cargo test --workspace            # unit, property/fuzz, HTTP, fake-LLM and migration tests
cargo clippy --workspace --all-targets
zig test zig/src/native.zig       # also -O ReleaseSafe / ReleaseFast
node scripts/js-smoke.mjs         # dashboard scripts vs. a fake DOM with hostile data
cargo audit
curl -X POST -H 'X-Requested-With: tgh' http://127.0.0.1:8787/api/selftest   # live: DB, LLM chain, Telegram, bot e2e
```

The live self-test sends only read-only commands (`/status`, `/help`, `/settings`, `/topics`, `/search`, one free-text question)
from your account to your own bot and checks the replies.

## Origin and license

A Rust rewrite of [Magerko/TelegramHelper](https://github.com/Magerko/TelegramHelper) (MIT). The intent-router prompts and the overall
feature set come from the original; the code, schema extensions, web UI, Zig library and tests are new. Licensed under MIT — see `LICENSE`.
Not affiliated with Telegram. Automating a user account is subject to Telegram's Terms of Service and API rules; use your own
`api_id`/`api_hash` and keep request rates reasonable.

