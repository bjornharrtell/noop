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
the unit, so a machine left running both replies in chat and claims tasks. An owner message decides what it is: a question
is answered in two or three sentences; real work becomes a task in `tasks/inbox/` with the owner's wording as its details,
and the answer says which task it is (`On it: T-0007 …`). Merging, releasing and credentials stay the owner's call: the
assistant still creates the task and its run asks. `noop watch` drains the server's inbox with a saved cursor, holding the
request open up to 25 seconds when the inbox is empty, so an idle machine asks rarely. A failure goes to the journal, never
to the owner as a raw error. The service unit runs as the user whose `copilot` login the runs use.

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

### Where this port came from

`rust/` was written against **`54263a0c`** of [huacnlee/noop](https://github.com/huacnlee/noop) — *ci: Key the NSH
cache by runner image (#389)* — by reading that revision: the Worker in `cloud/`, the daemon in `src/`, and the tests. The
TypeScript daemon and Worker are superseded by `crates/noop-agent` and `crates/noop-server`; the app in `app/`, the site
in `website/` and the deploy files still ship as upstream writes them.

Upstream was reviewed at **`2fa1a9b`** (*app: Update phones over the air and add Check for updates (#419)*), 29 commits
after the base, and merged into this branch: `app/App.tsx` was the only file both sides had touched and it merged
cleanly, so the rebuilt web client now carries those 29 commits' client work (channel history, phone updates, the
primary-machine mark, the IP allow-list editor, dictation). Three of our own client changes survive it: the `fetch`
handler in `app/web/sw.js` (Chrome's install requirement) and the always-there menu button in `app/App.tsx`.

Behavior was ported from these commits, with a test each:

| upstream | what it fixed | here |
|---|---|---|
| `ebcb58b` | `every = "1h"` / `"1d"` were read as clock times, so three routines were dead for two weeks; an unknown schedule is now reported once instead of failing silently | `routines::interval_minutes`, `valid_every`, the journal warning |
| `7e14ce6` | a stale claim requeued over an already-queued copy reset its failure count, so a task could retry forever | `supervisor::reap` drops the stale copy instead |
| `98f120d` | `locks/` synced between machines; deletions had to land before writes; `x` ↔ `x/y` flips | `lib.rs`: `ignored`, deletion-first `scan`, directory handling in `pull` |
| `1ac7e6a` | directory listings read every tombstone; rewrites rewrote index rows | `files_live` partial index, update-in-place in `apply_cas` |
| `7dcdcf3` | the app marks the primary machine wherever machines are listed | `primary` in the heartbeat's host entry |
| `8595025` | channel-started results were routed back into the channel | nothing to do: this server never tagged them |

Not ported, deliberately: NsMetry analytics (`edf00e8`, `40ac254`, an external service), task grading (`95573f4`,
`87d24be`, `46596d3`, `68bd950`, `06f0c44`, model-driven and not designed for Copilot), the Windows service and installer
(`921b366`, `cf1a4c7`, `f0a3c93`, the agent is Linux systemd only), Claude and Codex login and quota handling
(`fde5837`, `c7bda32`, `c4665ca`, `7200b9c`; the agent runs Copilot), and per-repo worktrees (`9145f41`).

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
