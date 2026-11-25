//! noop-server: run with HUB_TOKEN (and optionally HUB_OWNER_EMAIL, HUB_SMTP, HUB_LANG) in the environment.
//!   NOOP_DB      SQLite file (default ./noop.db)
//!   NOOP_LISTEN  address to listen on (default 127.0.0.1:8799; put a TLS reverse proxy such as Caddy in front)
use std::net::SocketAddr;

use noop_server::app::{App, Config};

#[tokio::main]
async fn main() {
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
    let db = std::env::var("NOOP_DB").unwrap_or_else(|_| "noop.db".into());
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
