use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast::error::RecvError, mpsc};

use crate::room::Kind;
use crate::spatial::{self, Member};
use crate::state::{App, CHANNELS, ClientStats, RATE};
use crate::{audio, clock};

const RECEIVER_HTML: &str = include_str!("../web/receiver.html");
const RECEIVER_JS: &str = include_str!("../web/receiver.js");
const DASHBOARD_HTML: &str = include_str!("../web/dashboard.html");
const DASHBOARD_JS: &str = include_str!("../web/dashboard.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(receiver_page))
        .route("/receiver.js", get(|| async { asset("text/javascript", RECEIVER_JS) }))
        .route("/dashboard.js", get(|| async { asset("text/javascript", DASHBOARD_JS) }))
        .route("/style.css", get(|| async { asset("text/css", STYLE_CSS) }))
        .route("/host", get(dashboard))
        .route("/qr.svg", get(qr))
        .route("/ws", get(receiver_ws))
        .route("/ws/control", get(control_ws))
        .with_state(app)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn receiver_page(ConnectInfo(addr): ConnectInfo<SocketAddr>, State(app): State<Arc<App>>) -> Html<&'static str> {
    // Seeing this line proves the device can reach us; not seeing it means the network blocks it.
    if !app.is_local(addr.ip()) {
        println!("  {} opened the page", addr.ip());
    }
    Html(RECEIVER_HTML)
}

async fn dashboard(ConnectInfo(addr): ConnectInfo<SocketAddr>, State(app): State<Arc<App>>) -> Response {
    if !app.is_local(addr.ip()) {
        return (StatusCode::FORBIDDEN, "The dashboard is only available on the host PC.").into_response();
    }
    Html(DASHBOARD_HTML).into_response()
}

async fn qr(Query(q): Query<HashMap<String, String>>) -> Response {
    let text = q.get("text").map(String::as_str).unwrap_or("");
    match qrcode::QrCode::new(text.as_bytes()) {
        Ok(code) => {
            let svg = code
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(220, 220)
                .quiet_zone(true)
                .build();
            ([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response()
        }
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}

/// Receiver page → host.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ReceiverMsg {
    Hello { device_id: String, kind: Kind },
    Ping { c: f64 },
    Start,
    Stop,
    Stats(ClientStats),
}

async fn receiver_ws(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app): State<Arc<App>>,
) -> Response {
    ws.on_upgrade(move |socket| receiver_session(socket, addr.ip(), app))
}

async fn receiver_session(socket: WebSocket, ip: IpAddr, app: Arc<App>) {
    // A browser on the host plays into our virtual sink (it's the default output), which
    // would feed the room's audio back into itself. The PC has its own output for that.
    let is_host = app.is_local(ip) && !app.allow_local_receivers;
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<String>();
    let id = app.add_client(ip, is_host, ctl_tx.clone());
    let hello = json!({ "type": "hello", "id": id, "is_host": is_host, "rate": RATE, "channels": CHANNELS });
    let _ = ctl_tx.send(hello.to_string());

    let (mut sink, mut stream) = socket.split();
    let streaming = Arc::new(AtomicBool::new(false));
    let mut audio_rx = app.audio_tx.subscribe();
    let writer = tokio::spawn({
        let streaming = streaming.clone();
        let app = app.clone();
        async move {
            loop {
                let msg = tokio::select! {
                    // Clock-sync replies go out ahead of queued audio.
                    biased;
                    m = ctl_rx.recv() => match m {
                        Some(text) => Message::Text(text.into()),
                        None => break,
                    },
                    a = audio_rx.recv() => match a {
                        Ok(chunk) if streaming.load(Relaxed) => Message::Binary(match &chunk.scene {
                            None => chunk.stereo_packet.clone(),
                            // 3D: this device's own mono share of the scene.
                            Some(scene) => {
                                let mono = spatial::mix(scene, &app.spatial.layout(), Member::Client(id), chunk.frames);
                                audio::packet(chunk.seq, chunk.t_play_us, 1, &mono)
                            }
                        }),
                        Ok(_) | Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    },
                };
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => match serde_json::from_str::<ReceiverMsg>(&text) {
                Ok(ReceiverMsg::Ping { c }) => {
                    let _ = ctl_tx.send(json!({ "type": "pong", "c": c, "h": clock::now_us() }).to_string());
                }
                Ok(ReceiverMsg::Hello { device_id, kind }) => app.identify_client(id, device_id, kind),
                Ok(ReceiverMsg::Start) => {
                    if !is_host && !streaming.swap(true, Relaxed) {
                        println!("  {ip} joined");
                        app.set_joined(id, true);
                    }
                }
                Ok(ReceiverMsg::Stop) => {
                    streaming.store(false, Relaxed);
                    app.set_joined(id, false);
                }
                Ok(ReceiverMsg::Stats(stats)) => app.update_client(id, stats),
                Err(_) => {}
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
    writer.abort();
    app.remove_client(id);
    if streaming.load(Relaxed) {
        println!("  {ip} left");
    }
}

async fn control_ws(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app): State<Arc<App>>,
) -> Response {
    if !app.is_local(addr.ip()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| control_session(socket, app))
}

async fn control_session(socket: WebSocket, app: Arc<App>) {
    let (mut sink, mut stream) = socket.split();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let state = app.snapshot().to_string();
                if sink.send(Message::Text(state.into())).await.is_err() {
                    break;
                }
            }
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => app.apply_command(&text),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
        }
    }
}
