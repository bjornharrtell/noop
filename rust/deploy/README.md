# Deploying noop-server on a small VPS

Debian or Ubuntu, x86_64 or ARM, 1 GB RAM, a DNS name pointing at the VPS (on Hetzner the server's own name works,
e.g. `static.218.116.29.2.clients.your-server.de`). noop-server listens on localhost; Caddy terminates HTTPS and gets
the certificate itself. The same steps are on the project page: <https://bjornharrtell.github.io/noop/>.

## 1. Install

```bash
curl -fsSL https://bjornharrtell.github.io/noop/install.sh | sudo bash -s -- \
  --domain noop.example.com --email you@example.com --firewall
```

It downloads the latest release (server binary, web app, checksums verified), creates the `noop` user, writes
`/etc/noop/noop.env` with a new machine key and Web Push keys, installs the systemd units (server and nightly backup),
installs Caddy for `--domain` and, with `--firewall`, allows only SSH, HTTP and HTTPS. Leave out `--firewall` on a
server that runs other things; a Caddyfile that serves other sites is left alone (the script prints the block to add).

## 2. First sign-in

Signing in takes your email, a one-time code and the code from an authenticator app.

1. **Who may sign in.** Set at install with `--email`; to change it later, run the installer again with `--email`, or
   `sudoedit /etc/noop/noop.env` (`HUB_OWNER_EMAIL=you@example.com`) and `sudo systemctl restart noop-server`.
2. **Bind your authenticator app** (Google Authenticator, 1Password, Authy…), on the VPS:
   ```bash
   sudo -u noop noop-server 2fa-setup
   ```
   Scan the QR code, then type the 6-digit code the app shows. Running it again replaces the authenticator.
3. **Open your Noop address**, enter your email, press *Send code*.
4. **Get the one-time code:**
   - without email set up: `sudo -u noop noop-server login-code` on the VPS;
   - with email set up (below): from your mailbox.
5. Enter that code and the authenticator's current code. On a phone, add the page to the home screen and allow
   notifications.

Every new sign-in is announced in the feed (and by email when it is set up); `noop revoke-sessions` on a machine signs
everyone out.

### Codes by email (optional)

`sudoedit /etc/noop/noop.env` and set `HUB_SMTP` on one line, for example with a Gmail
[app password](https://myaccount.google.com/apppasswords):

```bash
HUB_SMTP={"host":"smtp.gmail.com","port":465,"user":"you@gmail.com","pass":"abcd efgh ijkl mnop","from":"Noop <you@gmail.com>"}
```

Port 465 is TLS, 587 STARTTLS. Then `sudo systemctl restart noop-server` and check it with
`sudo -u noop noop-server test-mail you@example.com`.

## 3. Connect the machines

Machines use the address (`https://noop.example.com`) and the machine key, `HUB_TOKEN` in `/etc/noop/noop.env`
(`sudo sed -n 's/^HUB_TOKEN=//p' /etc/noop/noop.env`). On each machine set `[cloud] url` in `~/.noop/noop.toml` and
the token, then restart the machine program. Do not run `noop cloud deploy` any more: it deploys the Cloudflare
Worker.

## Moving from Cloudflare

Copy the data before you start using the new server (the Worker keeps serving meanwhile), and use the Worker's
`HUB_TOKEN` as the new server's (secrets are sealed with it): put it in `/etc/noop/noop.env`, then

```bash
sudo systemctl stop noop-server
sudo -u noop env NOOP_IMPORT_TOKEN=<the Worker's HUB_TOKEN> noop-server import-from https://<your-worker>.workers.dev
sudo systemctl start noop-server
```

Signed-in browsers, the 2FA binding, files, history, the feed, secrets and media come over.

## Updating

```bash
curl -fsSL https://bjornharrtell.github.io/noop/install.sh | sudo bash
```

Replaces the server and the web app; settings and data are kept. `--version server-vX.Y.Z` installs a given release.

## Backups and restore

`noop-backup.timer` writes `/var/lib/noop/backups/noop-YYYYMMDD.db` nightly and keeps 14; copy them off the VPS too.
`sudo -u noop noop-server backup <file>` makes one at any time. Restore: `systemctl stop noop-server`, copy a backup
over `/var/lib/noop/noop.db` (delete `noop.db-wal` and `noop.db-shm`), `systemctl start noop-server`.

## Releases

A tag `server-vX.Y.Z` runs `.github/workflows/server-release.yml`: it tests and builds the server for x86_64 and
aarch64 (on Ubuntu 22.04, so it runs on any newer glibc), builds the web app with NSH, publishes them with
`SHA256SUMS` as a GitHub release, and deploys this installer and `rust/pages/` to GitHub Pages.

## By hand

The files the installer uses are here: `noop-server.service`, `noop-backup.service`, `noop-backup.timer`,
`Caddyfile`, `noop.env.example`. Build with `cd rust && cargo build --release`, the web app with `app/build-web.sh`
(needs NSH), and follow `install.sh` step by step.
