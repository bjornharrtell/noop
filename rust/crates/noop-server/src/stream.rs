//! Live sockets. Port of the Worker's hibernating WebSockets:
//!
//!   WS /api/stream[?v=2]  owner clients: every new feed event as JSON; with v=2 also `{"$status": …}` frames, so the
//!                         app need not poll /api/status. "ping" is answered "pong".
//!   WS /api/host/stream   the primary (machine key): `{"type":"inbox"}` per inbox item instead of a long-poll, and
//!                         `{"type":"watchers","n":…}` when the number of owner sockets changes.
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::response::Response;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::app::{err, App, Frame, Req};
use crate::auth::Who;
use crate::feed::{status_view, touch_host};

/// The primary pushes live agent activity only while someone looks: tell it when that changes.
pub fn tell_watchers(app: &App) {
    let _ = app.host_tx.send(Arc::from(json!({ "type": "watchers", "n": app.owner_sockets() }).to_string()));
}

/// Owner clients that asked for status frames (?v=2) get the dashboard pushed.
pub fn push_status(app: &App, c: &rusqlite::Connection) {
    let _ = app.owner_tx.send(Frame::Status(Arc::from(json!({ "$status": status_view(app, c) }).to_string())));
}

pub fn open(app: &Arc<App>, req: &Req, w: &Who) -> Response {
    let host = req.path == "/api/host/stream";
    if host && *w != Who::Machine {
        return err("forbidden", 403);
    }
    let Some(ws) = req.ws.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return err("websocket expected", 426);
    };
    let app = app.clone();
    if host {
        ws.on_upgrade(move |socket| host_socket(app, socket))
    } else {
        let v2 = req.q("v") == Some("2");
        ws.on_upgrade(move |socket| owner_socket(app, socket, v2))
    }
}

/// "ping" → "pong" (the Worker's auto-response); false when the socket is done.
async fn incoming(socket: &mut WebSocket, msg: Option<Result<Message, axum::Error>>) -> bool {
    match msg {
        Some(Ok(Message::Text(t))) if t.as_str() == "ping" => socket.send(Message::Text("pong".into())).await.is_ok(),
        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => false,
        Some(Ok(_)) => true,
    }
}

async fn owner_socket(app: Arc<App>, mut socket: WebSocket, v2: bool) {
    let mut rx = app.owner_tx.subscribe();
    app.add_owner_socket(1);
    tell_watchers(&app);
    loop {
        tokio::select! {
            frame = rx.recv() => {
                let text = match frame {
                    Ok(Frame::Event(s)) => s,
                    Ok(Frame::Status(s)) if v2 => s,
                    Ok(Frame::Status(_)) | Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                };
                if socket.send(Message::Text(text.as_ref().into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => if !incoming(&mut socket, msg).await { break },
        }
    }
    app.add_owner_socket(-1);
    tell_watchers(&app);
}

async fn host_socket(app: Arc<App>, mut socket: WebSocket) {
    let mut rx = app.host_tx.subscribe();
    app.add_host_socket(1);
    {
        let c = app.db();
        if let Err(e) = touch_host(&app, &c) {
            tracing::error!("host stream: {e}");
        }
        push_status(&app, &c);
    }
    tell_watchers(&app);
    loop {
        tokio::select! {
            frame = rx.recv() => {
                let text = match frame {
                    Ok(s) => s,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                };
                if socket.send(Message::Text(text.as_ref().into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => if !incoming(&mut socket, msg).await { break },
        }
    }
    app.add_host_socket(-1);
    let c = app.db();
    let _ = touch_host(&app, &c);
    push_status(&app, &c);
}
