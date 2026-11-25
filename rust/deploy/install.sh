#!/usr/bin/env bash
# Install or update the Noop server on a Linux VPS (Debian/Ubuntu, x86_64 or aarch64), from the latest GitHub release.
#
#   curl -fsSL https://bjornharrtell.github.io/noop/install.sh | sudo bash -s -- --domain noop.example.com --email you@example.com
#
# Run it again (without options) to update: the binary and the web app are replaced, settings and data are kept.
#   --domain NAME   serve https://NAME with Caddy (installed if missing); its DNS must point at this server
#   --email ADDR    who may sign in (sets HUB_OWNER_EMAIL; comma-separated for several)
#   --firewall      turn on ufw, allowing only SSH, HTTP and HTTPS
#   --version TAG   install a given release (e.g. server-v0.1.0) instead of the latest
set -euo pipefail

REPO="${NOOP_REPO:-bjornharrtell/noop}"
DOMAIN="" EMAIL="" FIREWALL=0 VERSION="latest"
while [ $# -gt 0 ]; do
  case "$1" in
    --domain) DOMAIN="$2"; shift 2 ;;
    --email) EMAIL="$2"; shift 2 ;;
    --firewall) FIREWALL=1; shift ;;
    --version) VERSION="$2"; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0" 2>/dev/null || true; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1m%s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
[ "$(id -u)" = 0 ] || die "run as root (sudo bash)"
command -v curl >/dev/null || die "curl is needed"
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "unsupported CPU $(uname -m)" ;;
esac
if [ "$VERSION" = latest ]; then BASE="https://github.com/$REPO/releases/latest/download"; else BASE="https://github.com/$REPO/releases/download/$VERSION"; fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
say "Downloading $VERSION for $ARCH"
for f in "noop-server-$ARCH-linux.tar.gz" noop-web.tar.gz SHA256SUMS; do
  curl -fsSL --retry 3 -o "$TMP/$f" "$BASE/$f" || die "download $BASE/$f failed"
done
(cd "$TMP" && grep -E " (noop-server-$ARCH-linux|noop-web)\.tar\.gz$" SHA256SUMS | sha256sum -c --quiet -) || die "checksum mismatch"
mkdir -p "$TMP/server" && tar -xzf "$TMP/noop-server-$ARCH-linux.tar.gz" -C "$TMP/server"

say "Installing"
id noop >/dev/null 2>&1 || useradd --system --home /var/lib/noop --shell /usr/sbin/nologin noop
install -m 755 "$TMP/server/noop-server" /usr/local/bin/noop-server
mkdir -p /etc/noop /var/lib/noop/backups
# the web app: swapped in whole, so a browser never sees half of two versions
rm -rf /var/lib/noop/web.new && mkdir -p /var/lib/noop/web.new
tar -xzf "$TMP/noop-web.tar.gz" -C /var/lib/noop/web.new
if [ -d /var/lib/noop/web ]; then mv /var/lib/noop/web /var/lib/noop/web.old; fi
mv /var/lib/noop/web.new /var/lib/noop/web && rm -rf /var/lib/noop/web.old
chown -R noop:noop /var/lib/noop

ENV=/etc/noop/noop.env
if [ ! -f "$ENV" ]; then
  TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  VAPID="$(/usr/local/bin/noop-server vapid-keys)"
  cat > "$ENV" <<EOF
# Noop server settings. Edit with: sudoedit $ENV   then: sudo systemctl restart noop-server
# The machine key: machines use it to connect. Keep it secret.
HUB_TOKEN=$TOKEN
# Who may sign in (comma-separated).
HUB_OWNER_EMAIL=$EMAIL
# Sign-in codes by email (optional). Without it: sudo -u noop noop-server login-code
#HUB_SMTP={"host":"smtp.example.com","port":465,"user":"you@example.com","pass":"app-password","from":"Noop <you@example.com>"}
HUB_LANG=en
$VAPID
NOOP_DB=/var/lib/noop/noop.db
NOOP_LISTEN=127.0.0.1:8799
NOOP_WEB_DIR=/var/lib/noop/web
NOOP_CLIENT_IP_HEADER=x-forwarded-for
EOF
  say "Created $ENV with a new machine key"
elif [ -n "$EMAIL" ]; then
  if grep -q '^HUB_OWNER_EMAIL=' "$ENV"; then sed -i "s|^HUB_OWNER_EMAIL=.*|HUB_OWNER_EMAIL=$EMAIL|" "$ENV"; else echo "HUB_OWNER_EMAIL=$EMAIL" >> "$ENV"; fi
  say "Set HUB_OWNER_EMAIL=$EMAIL"
fi
chown root:noop "$ENV" && chmod 640 "$ENV"

install -m 644 "$TMP"/server/deploy/noop-server.service "$TMP"/server/deploy/noop-backup.service "$TMP"/server/deploy/noop-backup.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --quiet noop-server noop-backup.timer
systemctl restart noop-server
systemctl start noop-backup.timer

if [ -n "$DOMAIN" ]; then
  if ! command -v caddy >/dev/null; then
    say "Installing Caddy"
    DEBIAN_FRONTEND=noninteractive apt-get -qq update >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get -qq install -y caddy >/dev/null
  fi
  CADDYFILE=/etc/caddy/Caddyfile
  # only an empty one, the package's welcome page, or one written for Noop is replaced; another site's is left alone
  OURS=0
  if [ ! -s "$CADDYFILE" ] || grep -q 'managed by noop' "$CADDYFILE" || grep -q 'root \* /usr/share/caddy' "$CADDYFILE"; then
    OURS=1
  elif grep -q 'reverse_proxy 127.0.0.1:8799' "$CADDYFILE" && [ "$(grep -c '{$' "$CADDYFILE")" -le 2 ]; then
    OURS=1
  fi
  if [ "$OURS" = 1 ]; then
    cat > "$CADDYFILE" <<EOF
# managed by noop install.sh: HTTPS with an automatic certificate, everything proxied to noop-server.
$DOMAIN {
	encode zstd gzip
	reverse_proxy 127.0.0.1:8799 {
		# WebSockets and the inbox long-poll stay open
		flush_interval -1
	}
}
EOF
    caddy validate --config "$CADDYFILE" --adapter caddyfile >/dev/null 2>&1 || die "Caddy rejected $CADDYFILE"
    systemctl enable --quiet --now caddy
    systemctl reload caddy
  else
    say "$CADDYFILE serves other sites: add this block yourself, then: systemctl reload caddy"
    printf '%s {\n\treverse_proxy 127.0.0.1:8799 {\n\t\tflush_interval -1\n\t}\n}\n' "$DOMAIN"
  fi
fi

if [ "$FIREWALL" = 1 ]; then
  command -v ufw >/dev/null || DEBIAN_FRONTEND=noninteractive apt-get -qq install -y ufw >/dev/null
  ufw allow OpenSSH >/dev/null && ufw allow 80/tcp >/dev/null && ufw allow 443/tcp >/dev/null && ufw allow 443/udp >/dev/null
  ufw --force enable >/dev/null
  say "Firewall on: SSH, HTTP, HTTPS"
fi

sleep 1
code="$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8799/api/status || true)"
[ "$code" = 401 ] || die "noop-server is not answering (HTTP $code): journalctl -u noop-server -n 50"
say "✓ $(/usr/local/bin/noop-server version) is running"
OWNER="$(sed -n 's/^HUB_OWNER_EMAIL=//p' "$ENV")"
URL="https://${DOMAIN:-<your domain>}"
cat <<EOF

Next steps (first install):
  1. Who may sign in:   ${OWNER:-not set: sudo bash install.sh --email you@example.com  (or sudoedit $ENV)}
  2. Authenticator app: sudo -u noop noop-server 2fa-setup
  3. Sign in at $URL with your email, then the code from
       - your mailbox, if HUB_SMTP is set (check it: sudo -u noop noop-server test-mail you@example.com), or
       - sudo -u noop noop-server login-code
     and the 6-digit code from the authenticator app.
EOF
