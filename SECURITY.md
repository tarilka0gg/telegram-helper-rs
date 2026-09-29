# Security

## Threat model
telegram-helper-rs runs **as your own Telegram account** (userbot) and can read and send messages, so treat the
machine it runs on and its `.env` / database as sensitive.

- The dashboard has **no authentication** and only binds to loopback. It refuses other addresses unless `ALLOW_REMOTE_UI=1`
  is set — do not do that without a reverse proxy that adds authentication.
- Secrets (MTProto sessions of the userbot and the bot, LLM provider keys) are stored in SQLite **encrypted with Fernet**
  using `ENCRYPTION_KEY`. Losing the key means signing in again; leaking it together with the database exposes the account.
- Only `OWNER_TELEGRAM_ID` is served by the control bot. Anything visible to other people is sent only after a button confirmation.
- Message text and chat names are sent to the configured LLM provider when you use LLM features (summaries, digests,
  classification, smart auto-reply). Use a provider you trust, or leave those features off.
- Never commit `.env`, `data/`, `*.db` or session files (they are in `.gitignore`).

## Reporting a vulnerability
Please open a private security advisory on GitHub (or contact the maintainer) instead of a public issue.
Include steps to reproduce; no message content or credentials are needed.
