# Noop in Rust (work in progress)

A Rust port of Noop's backends, for a small VPS instead of Cloudflare Workers. The HTTP API stays identical to the
Worker's (`cloud/src`), so the existing apps and the TypeScript machine program can talk to this server unchanged.

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

To do, in order:
1. Feed and inbox: `/api/events`, `/api/feed`, `/api/messages`, `/api/host/state`, `/api/host/inbox` (long-poll),
   `/api/status`, `/api/settings`, `/api/fleet/update`, `/api/tasks*`, `/api/channels*` (needs `tools.ts`).
2. WebSockets: `/api/stream` (owner, `?v=2` status frames), `/api/host/stream` (primary's inbox nudges).
3. Media and uploads: `/api/media`, `/api/uploads`, `/media/<id>`, media pruning.
4. Web Push (VAPID, `aes128gcm`), `/api/push/*`.
5. Secrets (`/api/secrets*`), MCP OAuth client (`/api/mcp-oauth*`), guests (`/api/guest*`).
6. MCP server and OAuth 2.1 provider (`/mcp`, `/oauth/*`, `/.well-known/*`).
7. `/api/host/export|import`, `/api/usage`, `HUB_ALLOW_IPS`, static web client with precompressed assets.
8. Deployment: systemd unit, Caddy example, backups (Litestream or nightly `.backup`).

Not ported on purpose: Cloudflare free-plan metering and shedding (`FREE`, `TARGET`, `LEAN`, `SHED`),
`HUB_MOVED_TO`, Cloudflare Email Routing.

## Machine program

Not started. Planned after the server runs a real fleet: `noop-core` gains sync, tasks and tools; a `noop` binary
replaces `src/`.
