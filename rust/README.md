# Noop in Rust (work in progress)

A Rust port of Noop's backends, for a small VPS instead of Cloudflare Workers. The HTTP API stays identical to the
Worker's (`cloud/src`), so the web client and the TypeScript machine program can talk to this server unchanged.

The only client is the web app: the NSH package built for the web target (WebAssembly), served by this server
from `NOOP_WEB_DIR`. Owners open the server's address in a browser (and can add it to the home screen); no native
desktop or mobile app is built or installed. Machines ("workers") run the machine program and only call the server.

```bash
app/build-web.sh                      # needs NSH; writes app/dist/web/noop
NOOP_WEB_DIR=app/dist/web/noop ...    # the server serves it at /
```

| Crate | What |
|---|---|
| `crates/noop-core` | Logic shared by server and machine program (paths, TOTP, login codes, sync pacing). No I/O. |
| `crates/noop-server` | The server: axum + one SQLite connection (WAL). Replaces the Worker and its Durable Object. |

## Run

```bash
cargo build --release
HUB_TOKEN=<machine key> HUB_OWNER_EMAIL=you@example.com NOOP_DB=/var/lib/noop/noop.db ./target/release/noop-server
```

| Variable | Meaning |
|---|---|
| `HUB_TOKEN` | Machine (fleet) credential. Required. |
| `HUB_OWNER_EMAIL` | Owner allow-list, comma-separated. |
| `HUB_SMTP` | JSON `{host, port, secure, starttls, user, pass, from}`. Without it, login codes go to `noop login`. |
| `HUB_LANG` | `en` for English text, otherwise Simplified Chinese. |
| `NOOP_DB` | SQLite file (default `./noop.db`). |
| `NOOP_LISTEN` | Listen address (default `127.0.0.1:8799`). Put a TLS proxy (Caddy) in front. |
| `NOOP_CLIENT_IP_HEADER` | Header the proxy puts the client IP in (e.g. `x-forwarded-for`), for security alerts. |
| `NOOP_WEB_DIR` | The web client package (`app/dist/web/noop`). Unknown paths get its `index.html`. |
| `HUB_VAPID_PUBLIC`, `HUB_VAPID_PRIVATE` | Web Push keys. Reuse the Worker's values (in `~/.noop/vapid.json`) to keep browsers subscribed, or run `noop-server vapid-keys` once. |

`HUB_ALLOW_IPS` is refused at startup until it is ported; restrict access at the firewall meanwhile.

## Test

```bash
cargo test            # unit tests + API contract tests (port of cloud/test/run.ts)
cargo clippy --all-targets
```

## Port status (server)

Done:
- Schema: all 22 Worker tables, same columns (ready for `/api/host/export` → `/api/host/import`).
- `/api/files/*`: changes, commit (compare-and-swap), history, revert, list, read; history pruning.
- `/api/auth/start|verify|logout`, sessions, idle expiry, daily failure lockout, security alerts (feed + SMTP).
- `/api/host/2fa`, `/2fa/setup`, `/2fa/confirm`, `/login-code`, `/join`, `/revoke-sessions`.
- Feed and inbox: `/api/events`, `/api/feed`, `/api/messages`, `/api/host/state`, `/api/host/inbox` (long-poll),
  `/api/status`, `/api/settings`, `/api/fleet/update`, `/api/tasks*`, `/api/channels*`.
- Web client at `/` from `NOOP_WEB_DIR`, precompressed `.br`/`.gz` for hashed assets, SPA fallback.
- WebSockets: `/api/stream` (feed events; `?v=2` adds `$status` frames), `/api/host/stream` (inbox nudges,
  watcher counts; an open socket keeps the primary online).
- Media: `/api/media` (≤ 30 MB), `/api/uploads` (owner images/Markdown, sniffed, ≤ 10 MB), `/media/<id>` with
  byte ranges and sandboxed headers; pruning by `media_days`.
- Web Push (RFC 8291 `aes128gcm`, RFC 8292 VAPID), `/api/push/*`; feed events notify per `notify_prefs`
  (kinds, quiet hours in the owner's time zone). `noop-server vapid-keys` prints a key pair.
- Shared secrets `/api/secrets*`: sealed exactly as the Worker sealed them, so imported secrets open.
- Signing in to remote MCP servers `/api/mcp-oauth*` (discovery, dynamic registration, PKCE, refresh before
  handing a token to a machine). The redirect URI uses `X-Forwarded-Proto`/`X-Forwarded-Host` from the proxy.
- Guest workers `/api/guest/*`, `/api/guests/*`.

To do, in order:
1. MCP server and OAuth 2.1 provider (`/mcp`, `/oauth/*`, `/.well-known/*`).
2. `/api/host/export|import`, `/api/usage`, `HUB_ALLOW_IPS`.
3. Deployment: systemd unit, Caddy example, backups (Litestream or nightly `.backup`).

Not ported on purpose: Cloudflare free-plan metering and shedding (`FREE`, `TARGET`, `LEAN`, `SHED`),
`HUB_MOVED_TO`, Cloudflare Email Routing, `/api/updates/direct/*` (native app updates; there are no native apps).

## Machine program

Not started. Planned after the server runs a real fleet: `noop-core` gains sync, tasks and tools; a `noop` binary
replaces `src/`.
