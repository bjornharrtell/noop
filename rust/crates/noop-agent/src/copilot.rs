//! One GitHub Copilot session per run, with Noop's gates as its permission handler.
//!
//! The SDK hosts the Copilot runtime in this process (its own docs: no crash isolation, no sandbox), so every refusal
//! here is the only thing standing between an agent and a release, a merge or your credentials. Authentication is the
//! machine's own `copilot` login; nothing is read from the environment here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use copilot_sdk::handler::{PermissionHandler, PermissionResult};
use copilot_sdk::types::{CreateSessionOptions, MessageOptions, PermissionRequestData, RequestId, SessionId};
use copilot_sdk::{
    CliProgram, Client, ClientMode, ClientOptions, RuntimeConnection, SpawnedProcessOptions,
};

/// Where the machine's own Copilot CLI is: `COPILOT_CLI_PATH`, else the first `copilot` on PATH.
///
/// The SDK is built without its bundled runtime, so the CLI the machine is logged in to is the one that runs; nothing is
/// downloaded and the binary stays small.
fn copilot_cli() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("COPILOT_CLI_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("COPILOT_CLI_PATH={} is not a file", p.display()));
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("copilot");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err("the GitHub Copilot CLI was not found on PATH: install it (curl -fsSL https://gh.io/copilot-install | bash) and log in once by running `copilot`".into())
}

use crate::gates;

/// What one run produced.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The assistant's final text.
    pub text: String,
    /// Refused requests, with the reason, for the journal.
    pub blocked: Vec<String>,
}

/// The permission handler: refuse what the gates refuse, approve the rest.
struct Gates {
    cwd: PathBuf,
    blocked: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl PermissionHandler for Gates {
    async fn handle(&self, _session: SessionId, _request: RequestId, data: PermissionRequestData) -> PermissionResult {
        match gates::blocked_request(&data.extra, &self.cwd) {
            Some(reason) => {
                let what = data.tool_call_id.clone().unwrap_or_else(|| "a request".into());
                self.blocked.lock().map(|mut b| b.push(format!("{what}: {reason}"))).ok();
                PermissionResult::reject(Some(reason.to_string()))
            }
            None => PermissionResult::approve_once(),
        }
    }
}

/// Run one prompt in `cwd` and return the assistant's answer.
pub async fn run(prompt: &str, cwd: &Path, model: Option<&str>, timeout: Duration) -> Result<Outcome, String> {
    let program = copilot_cli()?;
    let runtime = RuntimeConnection::SpawnedStdio(SpawnedProcessOptions {
        program: CliProgram::Path(program),
        working_directory: cwd.to_path_buf(),
        ..Default::default()
    });
    let client = Client::start(ClientOptions::new(ClientMode::CodingAgent).with_runtime(runtime))
        .await
        .map_err(|e| format!("copilot: {e} (is the `copilot` CLI logged in on this machine?)"))?;

    let blocked = Arc::new(Mutex::new(Vec::new()));
    let mut cfg = {
        let handler = Gates { cwd: cwd.to_path_buf(), blocked: blocked.clone() };
        let mut c = CreateSessionOptions::default().with_permissions(move |group| group.with_on_request(Arc::new(handler)));
        c.working_directory = Some(cwd.to_path_buf());
        c.model = model.map(str::to_string);
        c.events.streaming = Some(true);
        c
    };
    cfg.client_name = Some("noop".into());

    let session = client.create_session(cfg).await.map_err(|e| format!("copilot session: {e}"))?;

    // streaming deltas are the one shape the SDK documents for text; collect them while the turn runs
    let text = Arc::new(Mutex::new(String::new()));
    let mut events = session.subscribe();
    let sink = text.clone();
    let collector = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event.event_type.as_str() {
                "assistant.message_delta" => {
                    if let Some(d) = event.data.get("deltaContent").and_then(|v| v.as_str()) {
                        if let Ok(mut t) = sink.lock() {
                            t.push_str(d);
                        }
                    }
                }
                "session.idle" => break,
                _ => {}
            }
        }
    });

    let sent = session.send_and_wait(MessageOptions::new(prompt).with_wait_timeout(timeout)).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), collector).await;
    session.disconnect().await.ok();
    client.stop().await.ok();
    sent.map_err(|e| format!("copilot run: {e}"))?;

    let text = text.lock().map(|t| t.clone()).unwrap_or_default();
    let blocked = blocked.lock().map(|b| b.clone()).unwrap_or_default();
    Ok(Outcome { text, blocked })
}
