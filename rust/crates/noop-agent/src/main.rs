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

use std::path::{Path, PathBuf};

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

const USAGE: &str = "usage: noop <init <server url> --token <machine key> | pull | commit -m <message> | ask <prompt> | work [--once] | watch [--once] | decide <D-0001> <answer> | install-service [--now] | uninstall-service | self-update [--force] | status | version>";

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
                    if p.merged > 0 {
                        println!("merged {} journal/log file(s) written on two machines", p.merged);
                    }
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
        #[cfg(feature = "copilot")]
        Some("work") => {
            let c = ctx(&home);
            let agent = noop_agent::copilot::Copilot { model: Some(noop_agent::read_model(&home)), timeout: std::time::Duration::from_secs(1800) };
            let host = hostname();
            let once = args.iter().any(|a| a == "--once");
            loop {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                // housekeeping before taking work: routines that are due, claims nobody finished
                match noop_agent::routines::queue(&c, &host, now) {
                    Ok(n) if n > 0 => {
                        let _ = noop_agent::commit(&c, "routines").await;
                        println!("queued {n} routine task(s)");
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("routines: {e}"),
                }
                match noop_agent::supervisor::reap(&c, &host, now) {
                    Ok(n) if n > 0 => {
                        let _ = noop_agent::commit(&c, "release stale claims").await;
                        println!("released {n} stale claim(s)");
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("supervisor: {e}"),
                }
                match noop_agent::work::work_once(&c, &host, &agent).await {
                    Ok(Some(d)) => println!("{}: {}", d.id, d.result.lines().next().unwrap_or("").trim()),
                    Ok(None) => {
                        if once {
                            println!("nothing waiting");
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                    Err(e) => {
                        if once {
                            fail(e)
                        }
                        eprintln!("run failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                }
            }
        }
        #[cfg(not(feature = "copilot"))]
        Some("work") => fail("this build has no agent runner (built with --no-default-features)"),
        #[cfg(feature = "copilot")]
        Some("watch") => {
            let c = ctx(&home);
            let agent = noop_agent::copilot::Copilot { model: Some(noop_agent::read_model(&home)), timeout: std::time::Duration::from_secs(600) };
            let host = hostname();
            let once = args.iter().any(|a| a == "--once");
            loop {
                // the server holds the request up to 25 s when nothing is waiting, so an idle machine asks rarely
                match noop_agent::bridge::step(&c, &host, &agent, 25).await {
                    Ok(s) => {
                        if s.answered > 0 {
                            println!("answered {} message(s), cursor {}", s.answered, s.after);
                        }
                        if s.files {
                            println!("shared files changed (settings, channel or cancel); pulled");
                        }
                        if once {
                            break;
                        }
                    }
                    Err(e) => {
                        if once {
                            fail(e)
                        }
                        eprintln!("inbox: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    }
                }
            }
        }
        #[cfg(not(feature = "copilot"))]
        Some("watch") => fail("this build has no agent runner (built with --no-default-features)"),
        Some("install-service") => {
            let start = args.iter().any(|a| a == "--now");
            match install_service(&home, start) {
                Ok(m) => println!("{m}"),
                Err(e) => fail(e),
            }
        }
        Some("uninstall-service") => match uninstall_service() {
            Ok(m) => println!("{m}"),
            Err(e) => fail(e),
        },
        Some("decide") => {
            // answering from a shell until the app has a route for it
            let (Some(id), Some(answer)) = (args.get(1), args.get(2)) else {
                fail("usage: noop decide <D-0001> <answer>")
            };
            let c = ctx(&home);
            let who = std::env::var("SUDO_USER").unwrap_or_else(|_| hostname());
            match noop_agent::decisions::answer(&c, id, answer, &format!("owner ({who})"), "") {
                Ok(true) => match noop_agent::commit(&c, &format!("answer {id}")).await {
                    Ok(_) => println!("answered {id}: {answer}"),
                    Err(e) => fail(e),
                },
                Ok(false) => fail(format!("{id} is not open")),
                Err(e) => fail(e),
            }
        }
        Some("self-update") => {
            let force = args.iter().any(|a| a == "--force");
            match noop_agent::update::run(&home, env!("CARGO_PKG_VERSION"), force).await {
                Ok(noop_agent::update::Outcome::Current(v)) => println!("already on {v}"),
                Ok(noop_agent::update::Outcome::Updated { from, to }) => {
                    println!("updated {from} → {to}");
                    if std::env::var("INVOCATION_ID").is_ok() {
                        println!("restart the service to use it: systemctl restart noop-agent");
                    } else {
                        println!("start it again to use the new binary");
                    }
                }
                Err(e) => fail(e),
            }
        }
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

/// Remove the service, if it is installed.
fn uninstall_service() -> Result<String, String> {
    let unit = PathBuf::from("/etc/systemd/system/noop-agent.service");
    if !unit.is_file() {
        return Ok("noop-agent.service is not installed".into());
    }
    let run = |args: &[&str]| std::process::Command::new("systemctl").args(args).status();
    run(&["disable", "--now", "noop-agent"]).ok();
    std::fs::remove_file(&unit).map_err(|e| format!("remove unit: {e}"))?;
    run(&["daemon-reload"]).ok();
    Ok("stopped and removed noop-agent.service (the mirror in ~/.noop/data is untouched)".into())
}

/// Install the watch loop as a systemd service, running as the user whose `copilot` login the runs use.
fn install_service(home: &Path, start: bool) -> Result<String, String> {
    if !PathBuf::from("/run/systemd/system").is_dir() {
        return Err("this machine does not run systemd; start it yourself: noop watch".into());
    }
    let user = std::env::var("SUDO_USER").map_err(|_| {
        "run it with sudo, as the user whose `copilot` login the runs use: sudo noop install-service".to_string()
    })?;
    if user == "root" {
        return Err("log in to `copilot` as a normal user, then run: sudo noop install-service".into());
    }
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let home_dir = format!("/home/{user}");
    let unit = format!(
        "# /etc/systemd/system/noop-agent.service — the machine's Noop loop. Remove: sudo noop uninstall-service\n[Unit]\nDescription=Noop machine\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nUser={user}\nWorkingDirectory={h}\nEnvironment=NOOP_HOME={h}\nEnvironment=HOME={hd}\nExecStart={exe} watch\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=multi-user.target\n",
        h = home.display(),
        hd = home_dir,
        exe = exe.display(),
    );
    std::fs::write("/etc/systemd/system/noop-agent.service", unit).map_err(|e| format!("systemd unit: {e}"))?;
    let run = |args: &[&str]| std::process::Command::new("systemctl").args(args).status();
    run(&["daemon-reload"]).map_err(|e| format!("systemctl: {e}"))?;
    let mut enable = vec!["enable", "noop-agent"];
    if start {
        enable.push("--now");
    }
    run(&enable).map_err(|e| format!("systemctl enable: {e}"))?;
    Ok(format!(
        "installed /etc/systemd/system/noop-agent.service as {user}{}\n  check: systemctl status noop-agent    logs: journalctl -u noop-agent -f",
        if start { " (started)" } else { " (enabled; start it with: sudo systemctl start noop-agent)" }
    ))
}
