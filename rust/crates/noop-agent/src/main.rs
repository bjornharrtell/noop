//! noop: the program one machine runs. It mirrors the fleet's shared files to a local folder and keeps them in step with
//! the Noop server (`noop-server`, on a VPS).
//!
//!   noop init <server url> --token <machine key>   join a fleet from this machine
//!   noop pull                                      bring the mirror up to date
//!   noop commit -m <message>                       send local changes
//!   noop status                                    is the server answering, what is unsent
//!   noop version
//!
//! The mirror lives in `$NOOP_HOME/data` (default `~/.noop/data`); `noop.toml` beside it holds the server URL and the
//! machine key.

use std::path::PathBuf;

use noop_agent::Ctx;

fn home() -> PathBuf {
    if let Ok(h) = std::env::var("NOOP_HOME") {
        return PathBuf::from(h);
    }
    match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h).join(".noop"),
        Err(_) => PathBuf::from(".noop"),
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("noop: {msg}");
    std::process::exit(1);
}

const USAGE: &str = "usage: noop <init <server url> --token <machine key> | pull | commit -m <message> | ask <prompt> | status | version>";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let home = home();
    match args.first().map(String::as_str) {
        Some("version") | Some("--version") => println!("noop {}", env!("CARGO_PKG_VERSION")),
        Some("init") => {
            let Some(url) = args.get(1) else { fail(USAGE) };
            let Some(token) = args.iter().position(|a| a == "--token").and_then(|i| args.get(i + 1)) else {
                fail("init needs the machine key: noop init <server url> --token <key> (on the server: sed -n 's/^HUB_TOKEN=//p' /etc/noop/noop.env)")
            };
            let ctx = Ctx::new(url, token, &home);
            match ctx.call("GET", "/api/status", None).await {
                Ok((200, _)) => {}
                Ok((c, b)) => fail(format!("{} refused the machine key ({c} {}); check the URL and the key", ctx.url, b.get("error").and_then(|v| v.as_str()).unwrap_or(""))),
                Err(e) => fail(e),
            }
            let p = match noop_agent::write_config(&home, &ctx.url, &ctx.token) {
                Ok(p) => p,
                Err(e) => fail(e),
            };
            println!("joined {}\n  settings: {}\n  mirror:   {}", ctx.url, p.display(), ctx.mirror().display());
            match noop_agent::pull(&ctx).await {
                Ok(p) => {
                    println!("  pulled {} file(s), {} deleted", p.written, p.deleted);
                    if p.kept > 0 {
                        println!("  kept {} local edit(s); push them with: noop commit -m …", p.kept);
                    }
                }
                Err(e) => fail(e),
            }
        }
        Some("pull") => {
            let ctx = ctx(&home);
            match noop_agent::pull(&ctx).await {
                Ok(p) => {
                    println!("pulled {} file(s), {} deleted (rev {})", p.written, p.deleted, p.rev);
                    if p.kept > 0 {
                        println!("kept {} local edit(s) the server has newer versions of; push them with: noop commit -m …", p.kept);
                    }
                }
                Err(e) => fail(e),
            }
        }
        Some("commit") => {
            let msg = match args.iter().position(|a| a == "-m").and_then(|i| args.get(i + 1)) {
                Some(m) => m.clone(),
                None => format!("{}: sync", hostname()),
            };
            let ctx = ctx(&home);
            match noop_agent::commit(&ctx, &msg).await {
                Ok(c) if c.pushed == 0 => println!("nothing to push (rev {})", c.rev),
                Ok(c) => println!("pushed {} file(s) (rev {})", c.pushed, c.rev),
                Err(e) => fail(e),
            }
        }
        #[cfg(feature = "copilot")]
        Some("ask") => {
            let prompt = args[1..].join(" ");
            if prompt.trim().is_empty() {
                fail("usage: noop ask <prompt>  (runs one GitHub Copilot session in the mirror)");
            }
            let c = ctx(&home);
            let model = noop_agent::read_model(&home);
            match noop_agent::copilot::run(&prompt, &c.mirror(), Some(&model), std::time::Duration::from_secs(900)).await {
                Ok(o) => {
                    if !o.blocked.is_empty() {
                        eprintln!("refused {} request(s): {}", o.blocked.len(), o.blocked.join("; "));
                    }
                    println!("{}", o.text.trim());
                }
                Err(e) => fail(e),
            }
        }
        #[cfg(not(feature = "copilot"))]
        Some("ask") => fail("this build has no agent runner (built with --no-default-features)"),
        Some("status") => {
            let ctx = ctx(&home);
            match noop_agent::status(&ctx).await {
                Ok(s) => println!("{s}"),
                Err(e) => fail(e),
            }
        }
        _ => fail(USAGE),
    }
}

fn ctx(home: &PathBuf) -> Ctx {
    match noop_agent::read_config(home) {
        Ok((url, token)) => Ctx::new(url, token, home),
        Err(e) => fail(e),
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "machine".into())
}
