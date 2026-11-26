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
| `crates/noop-agent` | The machine program (`noop`): joins a fleet, mirrors the shared files, pushes local edits. Replaces the TypeScript client. |

## The machine program (`noop`)

```bash
noop init https://noop.example.com --token <machine key>   # once: writes ~/.noop/noop.toml (mode 600) and pulls
noop pull                                                  # fetch what other machines wrote
noop commit -m "what changed"                              # send local edits
noop work [--once]                                         # claim the next task, run it, record the outcome
noop watch [--once]                                        # answer the owner's chat messages
noop serve                                                 # both at once: what the service runs
noop status                                                # server reachable, file counts, what is unsent
sudo noop install-service --now                            # run it as a systemd service
noop self-update [--force]                                 # newest release, checksum-verified, replaced atomically
```

Before each work cycle the daemon queues the routines that are due (`[[routine]]` in `config.toml`: `every` is
`15m`, `hourly`, `daily HH:MM`, `weekdays HH:MM` or `weekly MON HH:MM`, in UTC; a machine that was asleep catches up)
and releases claims older than `fleet.reap_after_minutes`, so a machine that dies does not strand a task. Journals and
logs written on two machines merge by line union instead of one side winning. A settings change from the app is merged
into this machine's `config.toml` and shared.

`noop serve` runs the two loops together — answering the owner and taking work — and is what `install-service` puts in
the unit, so a machine left running both replies in chat and claims tasks. `noop watch` drains the server's inbox with a
saved cursor, holding the request open up to 25 seconds when the inbox is empty, so an idle machine asks rarely. An owner
message runs one Copilot session and the answer is posted back as a `notice`, which is what the chat shows; a failure goes
to the journal, never to the owner as a raw error. The service unit runs as the user whose `copilot` login the runs use.

A task is a Markdown file with front matter in `tasks/inbox/`. Claiming moves it to `tasks/active/` and commits it
once: the server's compare-and-swap is the lock, so of two machines that try at the same time exactly one wins and the
other puts its copy back. A finished run writes `tasks/done/`, a journal line (in the same commit, so it reaches the
other machines), and a feed event; a failed run releases the claim and records why.

The mirror is `~/.noop/data` (`NOOP_HOME` moves it); files are ordinary files, and `sync.json` beside it records the
server revision and each file's version. An edit this machine has not pushed yet survives a `pull`: the remote version
is recorded, the local bytes stay, and the next `commit` sends them on top of it. A rejected commit (another machine
wrote first) is retried the same way, so task claiming cannot silently overwrite a claim.

Still to port from the TypeScript client: the model-driven parts — the supervisor's verdicts, escalation cards and
decisions, and the learning, usage-report and proposal routines. Their prompts are written for claude/codex and have to
be rewritten for Copilot first.

### One Copilot session

```bash
copilot                       # once per machine: log in (it stores the credentials)
noop ask "summarise reports/" # one GitHub Copilot run in the mirror, answer on stdout
```

The runner uses the official Rust SDK (`copilot-sdk`), so authentication is the machine's own `copilot` login. The model
comes from `[agent] model` in `noop.toml` and defaults to `gpt-6-luna`; Copilot numbers its models itself, so if the
account does not offer that one the run fails with the runtime's message and the setting picks another. Each prompt
costs a premium request from the Copilot plan.

Noop's blocks are enforced as the session's permission handler, not by trusting the runtime (the SDK hosts the agent
in-process, with no sandbox): merging, releasing, publishing, force-pushing, deleting a repository, reading credentials,
and writing outside the run's working directory are refused, and every refusal is reported.

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

| `HUB_ALLOW_IPS` | Optional source-IP allow-list (IPs / CIDRs). Behind a proxy, set `NOOP_CLIENT_IP_HEADER` too. |

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
- `/api/host/export|import` (the Worker's shapes), `noop-server import-from <worker url>`, `/api/usage`
  (requests per route today), `HUB_ALLOW_IPS` (checked before every path, web client included).

- Deployment: [deploy/README.md](deploy/README.md) — hardened systemd unit, Caddy (HTTPS), nightly backups via
  `noop-server backup` (consistent while running), restore and update steps.

Skipped for now (owner's decision, 2026-11-25): the MCP server for Claude / ChatGPT connectors (`/mcp`, `/oauth/*`,
`/.well-known/*` answer 404). The web app does not use it.

Not ported on purpose: Cloudflare free-plan metering and shedding (`FREE`, `TARGET`, `LEAN`, `SHED`),
`HUB_MOVED_TO`, Cloudflare Email Routing, `/api/updates/direct/*` (native app updates; there are no native apps).

## Moving from Cloudflare

Stop nothing yet; the Worker keeps serving while you copy.

```bash
NOOP_DB=/var/lib/noop/noop.db NOOP_IMPORT_TOKEN=<the Worker's HUB_TOKEN> noop-server import-from https://<your-worker>
```

Every table is copied (and replaced) page by page: files and their history, the feed, sessions (so signed-in
browsers stay signed in), secrets, MCP sign-ins, push subscriptions, media. Run it again right before switching to
pick up the latest changes. Use the Worker's HUB_TOKEN on the new server too: secrets are sealed with it.

## Machine program

Not started. Planned after the server runs a real fleet: `noop-core` gains sync, tasks and tools; a `noop` binary
replaces `src/`.
