# Deploying noop-server on a small VPS

Tested shape: Debian or Ubuntu, 1 vCPU, 512 MB–1 GB RAM, a domain pointing at the VPS. noop-server listens on
localhost; Caddy terminates HTTPS and gets the certificate itself.

## 1. Build

On any Linux x86_64 machine with Rust (or on the VPS):

```bash
cd rust && cargo build --release
# → target/release/noop-server (one static-ish binary, ~10 MB)
```

The web client needs NSH, on any machine: `app/build-web.sh` → `app/dist/web/noop`.

## 2. Install

```bash
sudo useradd --system --home /var/lib/noop --shell /usr/sbin/nologin noop
sudo install -m 755 noop-server /usr/local/bin/noop-server
sudo mkdir -p /etc/noop /var/lib/noop/web
sudo cp -r app/dist/web/noop/. /var/lib/noop/web/
sudo chown -R noop:noop /var/lib/noop
sudo install -m 640 -o root -g noop rust/deploy/noop.env.example /etc/noop/noop.env
sudoedit /etc/noop/noop.env          # HUB_TOKEN, HUB_OWNER_EMAIL, SMTP, VAPID keys
sudo cp rust/deploy/noop-server.service rust/deploy/noop-backup.service rust/deploy/noop-backup.timer /etc/systemd/system/
```

Moving from Cloudflare: copy the data before the first start (the Worker keeps serving meanwhile):

```bash
sudo -u noop env NOOP_DB=/var/lib/noop/noop.db NOOP_IMPORT_TOKEN=<Worker HUB_TOKEN> \
  /usr/local/bin/noop-server import-from https://<your-worker>.workers.dev
```

## 3. Start

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now noop-server noop-backup.timer
sudo apt install caddy && sudo cp rust/deploy/Caddyfile /etc/caddy/Caddyfile && sudoedit /etc/caddy/Caddyfile
sudo systemctl reload caddy
curl -s https://noop.example.com/api/status     # → {"error":"unauthorized"}: up, and auth works
```

Firewall: only 22, 80 and 443 need to be open (`ufw allow OpenSSH; ufw allow 80,443/tcp; ufw enable`).

## 4. Point the machines at it

On each machine set `[cloud] url = "https://noop.example.com"` in `~/.noop/noop.toml` (and in the shared config, so new
machines get it), then restart the machine program. Do not run `noop cloud deploy` any more: it deploys the Worker.
For a last import with nothing lost, stop the machines, run `import-from` again, restart noop-server, then start the
machines on the new URL.

## Backups and restore

`noop-backup.timer` writes `/var/lib/noop/backups/noop-YYYYMMDD.db` nightly and keeps 14. Copy them off the VPS too
(e.g. `rsync` from another machine). `noop-server backup <file>` makes one by hand at any time.

Restore: `systemctl stop noop-server`, copy a backup over `/var/lib/noop/noop.db` (and delete `noop.db-wal`,
`noop.db-shm`), `systemctl start noop-server`.

## Updating

Build, `install` the new binary over `/usr/local/bin/noop-server`, `systemctl restart noop-server`. The schema is
created with `IF NOT EXISTS`, so an older database opens as is.
