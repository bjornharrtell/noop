//! noop-server: settings come from the environment (HUB_TOKEN, HUB_OWNER_EMAIL, HUB_SMTP, HUB_LANG, NOOP_DB, …), and
//! from /etc/noop/noop.env when it is readable (variables already set win).
//!   NOOP_DB      SQLite file (default ./noop.db)
//!   NOOP_LISTEN  address to listen on (default 127.0.0.1:8799; put a TLS reverse proxy such as Caddy in front)
//!
//! Subcommands (run as the service user on a VPS: `sudo -u noop noop-server …`):
//!   noop-server 2fa-setup                  bind the owner's authenticator app (QR code in the terminal)
//!   noop-server login-code                 show the pending sign-in code (when no SMTP is set up)
//!   noop-server test-mail <to>             send a test mail with HUB_SMTP
//!   noop-server vapid-keys                 print a new Web Push key pair (once, at setup)
//!   noop-server backup <file>              write a consistent copy of NOOP_DB (safe while the server runs)
//!   noop-server import-from <worker url>   copy a Cloudflare Worker's data into NOOP_DB (stop the server first);
//!                                          NOOP_IMPORT_TOKEN is the Worker's HUB_TOKEN
//!   noop-server version
use std::io::Write;
use std::net::SocketAddr;

use noop_server::app::{App, Config};
use noop_server::setup;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("noop-server: {msg}");
    std::process::exit(1);
}

fn open_db(db: &str) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(db).unwrap_or_else(|e| fail(format!("open {db}: {e}")));
    noop_server::db::init(&c).unwrap_or_else(|e| fail(format!("schema: {e}")));
    c
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str);
    if std::path::Path::new(setup::ENV_FILE).exists() {
        if let Err(e) = setup::load_env_file(setup::ENV_FILE) {
            // the service gets the file from systemd; for a subcommand it is a hint
            if cmd.is_some() {
                eprintln!("note: {e} (run as the noop user: sudo -u noop noop-server …)");
            }
        }
    }
    let db = std::env::var("NOOP_DB").unwrap_or_else(|_| "noop.db".into());
    match cmd {
        Some("version") | Some("--version") => {
            println!("noop-server {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("vapid-keys") => {
            // once, at setup: keep these, or every subscribed browser has to subscribe again
            let (public, private) = noop_server::push::new_vapid_keys();
            println!("HUB_VAPID_PUBLIC={public}\nHUB_VAPID_PRIVATE='{private}'");
            return;
        }
        Some("2fa-setup") => {
            let c = open_db(&db);
            let account = std::env::var("HUB_OWNER_EMAIL").ok().and_then(|e| e.split(',').next().map(|s| s.trim().to_string())).filter(|e| !e.is_empty()).unwrap_or_else(|| "owner".into());
            let secret = noop_core::totp::new_secret();
            let uri = noop_core::totp::otpauth_uri(&secret, &account, "Noop");
            println!("Scan this with your authenticator app (Google Authenticator, 1Password, Authy, …):\n");
            println!("{}", setup::qr_text(&uri));
            println!("Or enter this key by hand: {secret}\n");
            for _ in 0..3 {
                print!("Then type the 6-digit code it shows: ");
                std::io::stdout().flush().ok();
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
                    fail("no code typed");
                }
                match setup::bind_totp(&c, &secret, line.trim(), now_ms()) {
                    Ok(replaced) => {
                        println!("✓ Two-factor sign-in is on{}.", if replaced { " (the previous authenticator no longer works)" } else { "" });
                        println!("  Sign in at your Noop address with your email ({account}), the emailed code and this app's code.");
                        return;
                    }
                    Err(e) => eprintln!("{e}"),
                }
            }
            fail("not bound; run `noop-server 2fa-setup` again");
        }
        Some("login-code") => {
            let c = open_db(&db);
            match setup::pending_login_code(&c, now_ms()) {
                Some((email, code)) => println!("Sign-in code for {email}: {code}  (valid 10 minutes)"),
                None => fail("no pending code: on the sign-in page enter your email and press \"Send code\" first (and HUB_SMTP must be unset)"),
            }
            return;
        }
        Some("test-mail") => {
            let Some(to) = args.get(2) else { fail("usage: noop-server test-mail <address>") };
            let cfg = Config::from_env().unwrap_or_else(|e| fail(e));
            let Some(smtp) = cfg.smtp else { fail("HUB_SMTP is not set in /etc/noop/noop.env") };
            match noop_server::mail::send(&smtp, to, "Noop test mail", "This is a test mail from your Noop server. Mail works.\n").await {
                Ok(()) => println!("✓ sent to {to} via {}", smtp.host),
                Err(e) => fail(e),
            }
            return;
        }
        Some("backup") => {
            // a consistent copy while the server runs (WAL readers never block the writer)
            let Some(out) = args.get(2) else { fail("usage: noop-server backup <file>") };
            if let Err(e) = noop_server::ops::backup(&db, out) {
                fail(e);
            }
            println!("{out}");
            return;
        }
        Some("import-from") => {
            let (Some(url), Ok(token)) = (args.get(2), std::env::var("NOOP_IMPORT_TOKEN")) else {
                fail("usage: NOOP_IMPORT_TOKEN=<the Worker's HUB_TOKEN> noop-server import-from https://<worker>");
            };
            let mut conn = open_db(&db);
            if let Err(e) = noop_server::ops::import_from(&mut conn, url.trim_end_matches('/'), &token, |line| println!("{line}")).await {
                fail(format!("import: {e}"));
            }
            return;
        }
        Some(other) if other != "serve" => fail(format!("unknown command {other}; see `noop-server --help` in rust/README.md")),
        _ => {}
    }
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    ).init();
    let cfg = Config::from_env().unwrap_or_else(|e| {
        eprintln!("noop-server: {e}");
        std::process::exit(2);
    });
    let addr: SocketAddr = std::env::var("NOOP_LISTEN").unwrap_or_else(|_| "127.0.0.1:8799".into()).parse().unwrap_or_else(|e| {
        eprintln!("noop-server: NOOP_LISTEN: {e}");
        std::process::exit(2);
    });
    let app = App::new(open_db(&db), cfg, None).unwrap_or_else(|e| fail(format!("schema: {e}")));
    if let Err(e) = noop_server::serve(app, addr).await {
        fail(e);
    }
}
