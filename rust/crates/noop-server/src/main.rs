//! noop-server: run with HUB_TOKEN (and optionally HUB_OWNER_EMAIL, HUB_SMTP, HUB_LANG) in the environment.
//!   NOOP_DB      SQLite file (default ./noop.db)
//!   NOOP_LISTEN  address to listen on (default 127.0.0.1:8799; put a TLS reverse proxy such as Caddy in front)
//!
//! Subcommands:
//!   noop-server vapid-keys                 print a new Web Push key pair (once, at setup)
//!   noop-server backup <file>              write a consistent copy of NOOP_DB (safe while the server runs)
//!   noop-server import-from <worker url>   copy a Cloudflare Worker's data into NOOP_DB (stop the server first);
//!                                          NOOP_IMPORT_TOKEN is the Worker's HUB_TOKEN
use std::net::SocketAddr;

use noop_server::app::{App, Config};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("vapid-keys") {
        // once, at setup: keep these, or every subscribed browser has to subscribe again
        let (public, private) = noop_server::push::new_vapid_keys();
        println!("HUB_VAPID_PUBLIC={public}\nHUB_VAPID_PRIVATE='{private}'");
        return;
    }
    let db = std::env::var("NOOP_DB").unwrap_or_else(|_| "noop.db".into());
    if args.get(1).map(String::as_str) == Some("backup") {
        // a consistent copy while the server runs (WAL readers never block the writer)
        let Some(out) = args.get(2) else {
            eprintln!("usage: noop-server backup <file>");
            std::process::exit(2);
        };
        let r = rusqlite::Connection::open(&db).and_then(|c| c.execute("VACUUM INTO ?", [out]).map(|_| ()));
        if let Err(e) = r {
            eprintln!("noop-server: backup {db} → {out}: {e}");
            std::process::exit(1);
        }
        println!("{out}");
        return;
    }
    if args.get(1).map(String::as_str) == Some("import-from") {
        let (Some(url), Ok(token)) = (args.get(2), std::env::var("NOOP_IMPORT_TOKEN")) else {
            eprintln!("usage: NOOP_IMPORT_TOKEN=<the Worker's HUB_TOKEN> noop-server import-from https://<worker>");
            std::process::exit(2);
        };
        let mut conn = match rusqlite::Connection::open(&db).and_then(|c| noop_server::db::init(&c).map(|_| c)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("noop-server: open {db}: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = noop_server::ops::import_from(&mut conn, url.trim_end_matches('/'), &token, |line| println!("{line}")).await {
            eprintln!("noop-server: import: {e}");
            std::process::exit(1);
        }
        return;
    }
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    ).init();
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("noop-server: {e}");
            std::process::exit(2);
        }
    };
    let addr: SocketAddr = match std::env::var("NOOP_LISTEN").unwrap_or_else(|_| "127.0.0.1:8799".into()).parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("noop-server: NOOP_LISTEN: {e}");
            std::process::exit(2);
        }
    };
    let conn = rusqlite::Connection::open(&db).unwrap_or_else(|e| {
        eprintln!("noop-server: open {db}: {e}");
        std::process::exit(1);
    });
    let app = App::new(conn, cfg, None).unwrap_or_else(|e| {
        eprintln!("noop-server: schema: {e}");
        std::process::exit(1);
    });
    if let Err(e) = noop_server::serve(app, addr).await {
        eprintln!("noop-server: {e}");
        std::process::exit(1);
    }
}
