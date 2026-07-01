use crate::config::{AppConfig, RdpMode};
use crate::rdp::{ClientEvent, ServerEvent, ServerStatus, encode_video_frame, start_rdp_session};
use anyhow::Context;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::serve::Listener;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{self, ServerConfig};
use tokio_rustls::server::TlsStream;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, trace, warn};

const DIRECT_VIDEO_MEDIA: &str = r#"{"type":"media","video":{"h264":{"format":"annexb"}}}"#;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const SAVED_SESSION_EVENT_BUFFER: usize = 256;
const RDP_RECONNECT_NUDGE_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    saved_session: Arc<Mutex<Option<Arc<SavedRdpSession>>>>,
}

struct SavedRdpSession {
    control_tx: std::sync::mpsc::Sender<ClientEvent>,
    events_tx: broadcast::Sender<ServerEvent>,
    snapshot_rx: watch::Receiver<RdpSessionSnapshot>,
    done_rx: watch::Receiver<bool>,
}

#[derive(Clone, Debug, Default)]
struct RdpSessionSnapshot {
    connecting: bool,
    connected: bool,
    desktop_size: Option<(u32, u32)>,
    error: Option<String>,
    disconnected: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RdpQuery {
    width: Option<u16>,
    height: Option<u16>,
}

pub async fn serve(config: AppConfig) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.ui_bind)
        .await
        .with_context(|| format!("bind UI server on {}", config.ui_bind))?;
    let local_addr = listener.local_addr().context("read UI listen address")?;
    let app = app(config.clone());

    let acceptor = tls_acceptor(&config.tls).context("load UI TLS certificate")?;
    info!("serving rdp2web on https://{local_addr}");
    axum::serve(TlsListener::new(listener, acceptor), app)
        .await
        .context("serve HTTPS")
}

fn tls_acceptor(config: &crate::config::TlsConfig) -> anyhow::Result<TlsAcceptor> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let certs = load_certs(&config.cert_path)?;
    let key = load_private_key(&config.key_path)?;
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("configure TLS certificate")?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

fn load_certs(path: &std::path::Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let file =
        File::open(path).with_context(|| format!("open TLS certificate {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parse TLS certificate {}", path.display()))?;

    if certs.is_empty() {
        anyhow::bail!(
            "TLS certificate {} contains no certificates",
            path.display()
        );
    }

    Ok(certs)
}

fn load_private_key(path: &std::path::Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    let file =
        File::open(path).with_context(|| format!("open TLS private key {}", path.display()))?;
    let mut reader = BufReader::new(file);

    rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("parse TLS private key {}", path.display()))?
        .with_context(|| {
            format!(
                "TLS private key {} contains no supported private key",
                path.display()
            )
        })
}

struct TlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    handshakes_tx: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
    handshakes_rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    fn new(listener: TcpListener, acceptor: TlsAcceptor) -> Self {
        let (handshakes_tx, handshakes_rx) = mpsc::channel(128);
        Self {
            listener,
            acceptor,
            handshakes_tx,
            handshakes_rx,
        }
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, addr) = match accepted {
                        Ok(value) => value,
                        Err(err) => {
                            warn!(%err, "failed to accept TCP connection");
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            continue;
                        }
                    };

                    let acceptor = self.acceptor.clone();
                    let handshakes_tx = self.handshakes_tx.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                            Ok(Ok(stream)) => {
                                let _ = handshakes_tx.send((stream, addr)).await;
                            }
                            Ok(Err(err)) => {
                                debug!(%err, %addr, "failed TLS handshake");
                            }
                            Err(_) => {
                                debug!(%addr, "timed out TLS handshake");
                            }
                        }
                    });
                }
                handshake = self.handshakes_rx.recv() => {
                    if let Some((stream, addr)) = handshake {
                        return (stream, addr);
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

pub fn app(config: AppConfig) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/config", get(public_config))
        .route("/ws/rdp", get(rdp_ws))
        .layer(TraceLayer::new_for_http())
        .with_state(AppState {
            config: Arc::new(config),
            saved_session: Arc::new(Mutex::new(None)),
        })
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn public_config(State(state): State<AppState>) -> Json<crate::config::PublicConfig> {
    Json(state.config.public_config())
}

async fn rdp_ws(
    State(state): State<AppState>,
    Query(query): Query<RdpQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let rdp_config = match rdp_config_with_query(&state.config.rdp, &query) {
        Ok(config) => config,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };

    ws.max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| handle_rdp_socket(socket, state, rdp_config))
        .into_response()
}

fn rdp_config_with_query(
    base: &crate::config::RdpConfig,
    query: &RdpQuery,
) -> Result<crate::config::RdpConfig, &'static str> {
    let mut config = base.clone();
    if let Some(width) = query.width {
        config.width = width;
    }
    if let Some(height) = query.height {
        config.height = height;
    }

    if config.width < 200 || config.height < 200 {
        return Err("RDP width and height must be at least 200");
    }

    Ok(config)
}

async fn handle_rdp_socket(
    socket: WebSocket,
    state: AppState,
    rdp_config: crate::config::RdpConfig,
) {
    if rdp_config.mode == RdpMode::Session {
        handle_saved_rdp_socket(socket, state, rdp_config).await;
    } else {
        handle_transient_rdp_socket(socket, rdp_config).await;
    }
}

async fn handle_transient_rdp_socket(socket: WebSocket, rdp_config: crate::config::RdpConfig) {
    let (control_tx, mut server_rx) = start_rdp_session(rdp_config);
    let (mut ws_tx, mut ws_rx) = socket.split();

    let writer = tokio::spawn(async move {
        ws_tx
            .send(Message::Text(DIRECT_VIDEO_MEDIA.into()))
            .await
            .context("send direct-video media config")?;

        while let Some(event) = server_rx.recv().await {
            let result = match event {
                ServerEvent::Status(status) => {
                    let payload = serde_json::to_string(&status)?;
                    ws_tx.send(Message::Text(payload.into())).await
                }
                ServerEvent::Video(frame) => {
                    ws_tx
                        .send(Message::Binary(encode_video_frame(&frame).into()))
                        .await
                }
            };

            if let Err(err) = result {
                debug!(%err, "websocket client closed while sending event");
                break;
            }
        }

        anyhow::Ok(())
    });

    while let Some(message) = ws_rx.next().await {
        match message {
            Ok(Message::Text(text)) => match serde_json::from_str::<ClientEvent>(&text) {
                Ok(event) => {
                    if control_tx.send(event).is_err() {
                        break;
                    }
                }
                Err(err) => warn!(%err, "ignoring invalid client event"),
            },
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
            Err(err) => {
                warn!(%err, "websocket receive error");
                break;
            }
        }
    }

    drop(control_tx);
    if let Err(err) = writer.await {
        error!(%err, "websocket writer task failed");
    }
}

async fn handle_saved_rdp_socket(
    socket: WebSocket,
    state: AppState,
    rdp_config: crate::config::RdpConfig,
) {
    let session = saved_session(&state, rdp_config.clone()).await;
    let mut events_rx = session.events_tx.subscribe();
    let snapshot = session.snapshot_rx.borrow().clone();
    request_reconnect_size(
        &session.control_tx,
        &snapshot,
        rdp_config.width,
        rdp_config.height,
    );
    let control_tx = session.control_tx.clone();
    let (mut ws_tx, mut ws_rx) = socket.split();

    let writer = tokio::spawn(async move {
        ws_tx
            .send(Message::Text(DIRECT_VIDEO_MEDIA.into()))
            .await
            .context("send direct-video media config")?;
        send_saved_snapshot(&mut ws_tx, &snapshot).await?;

        loop {
            match events_rx.recv().await {
                Ok(event) => {
                    if let Err(err) = send_server_event(&mut ws_tx, event).await {
                        debug!(%err, "websocket client closed while sending saved-session event");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!(
                        skipped,
                        "websocket client lagged behind saved RDP session events"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }

        anyhow::Ok(())
    });

    while let Some(message) = ws_rx.next().await {
        match message {
            Ok(Message::Text(text)) => match serde_json::from_str::<ClientEvent>(&text) {
                Ok(event) => {
                    if control_tx.send(event).is_err() {
                        break;
                    }
                }
                Err(err) => warn!(%err, "ignoring invalid client event"),
            },
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
            Err(err) => {
                warn!(%err, "websocket receive error");
                break;
            }
        }
    }

    writer.abort();
    if let Err(err) = writer.await {
        if !err.is_cancelled() {
            error!(%err, "saved-session websocket writer task failed");
        }
    }
}

fn request_reconnect_size(
    control_tx: &std::sync::mpsc::Sender<ClientEvent>,
    snapshot: &RdpSessionSnapshot,
    width: u16,
    height: u16,
) {
    let current_size = snapshot.desktop_size.and_then(|(width, height)| {
        Some((u16::try_from(width).ok()?, u16::try_from(height).ok()?))
    });

    if snapshot.connected && current_size == Some((width, height)) {
        let nudge_width = nudge_width(width);
        let _ = control_tx.send(ClientEvent::Resize {
            width: nudge_width,
            height,
        });

        let control_tx = control_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RDP_RECONNECT_NUDGE_DELAY).await;
            let _ = control_tx.send(ClientEvent::Resize { width, height });
            let _ = control_tx.send(ClientEvent::Refresh { width, height });
        });
        return;
    }

    let _ = control_tx.send(ClientEvent::Resize { width, height });
    let _ = control_tx.send(ClientEvent::Refresh { width, height });
}

fn nudge_width(width: u16) -> u16 {
    if width > 202 { width - 2 } else { width + 2 }
}

async fn saved_session(
    state: &AppState,
    rdp_config: crate::config::RdpConfig,
) -> Arc<SavedRdpSession> {
    let mut saved_session = state.saved_session.lock().await;
    if let Some(session) = saved_session.as_ref() {
        if !*session.done_rx.borrow() {
            return session.clone();
        }
    }

    let (control_tx, mut server_rx) = start_rdp_session(rdp_config);
    let (events_tx, _) = broadcast::channel(SAVED_SESSION_EVENT_BUFFER);
    let (snapshot_tx, snapshot_rx) = watch::channel(RdpSessionSnapshot::default());
    let (done_tx, done_rx) = watch::channel(false);
    let session = Arc::new(SavedRdpSession {
        control_tx,
        events_tx: events_tx.clone(),
        snapshot_rx,
        done_rx,
    });

    tokio::spawn(async move {
        let mut snapshot = RdpSessionSnapshot::default();
        while let Some(event) = server_rx.recv().await {
            match &event {
                ServerEvent::Status(status) => {
                    trace!(?status, "broadcasting saved RDP status event");
                }
                ServerEvent::Video(frame) => {
                    trace!(
                        key = frame.key,
                        bytes = frame.data.len(),
                        subscribers = events_tx.receiver_count(),
                        "broadcasting saved RDP video event"
                    );
                }
            }
            update_saved_snapshot(&mut snapshot, &event);
            let _ = snapshot_tx.send(snapshot.clone());
            let _ = events_tx.send(event);
        }
        let _ = done_tx.send(true);
    });

    *saved_session = Some(session.clone());
    session
}

fn update_saved_snapshot(snapshot: &mut RdpSessionSnapshot, event: &ServerEvent) {
    match event {
        ServerEvent::Status(ServerStatus::Connecting) => {
            snapshot.connecting = true;
            snapshot.connected = false;
            snapshot.error = None;
            snapshot.disconnected = None;
        }
        ServerEvent::Status(ServerStatus::Connected) => {
            snapshot.connecting = false;
            snapshot.connected = true;
            snapshot.error = None;
            snapshot.disconnected = None;
        }
        ServerEvent::Status(ServerStatus::DesktopSize { width, height }) => {
            snapshot.desktop_size = Some((*width, *height));
        }
        ServerEvent::Status(ServerStatus::Disconnected { reason }) => {
            snapshot.connecting = false;
            snapshot.connected = false;
            snapshot.disconnected = Some(reason.clone());
        }
        ServerEvent::Status(ServerStatus::Error { message }) => {
            snapshot.connecting = false;
            snapshot.connected = false;
            snapshot.error = Some(message.clone());
        }
        ServerEvent::Video(_) => {}
    }
}

async fn send_saved_snapshot(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    snapshot: &RdpSessionSnapshot,
) -> anyhow::Result<()> {
    if let Some(message) = &snapshot.error {
        send_server_event(
            ws_tx,
            ServerEvent::Status(ServerStatus::Error {
                message: message.clone(),
            }),
        )
        .await?;
    } else if snapshot.connected {
        send_server_event(ws_tx, ServerEvent::Status(ServerStatus::Connected)).await?;
        if let Some((width, height)) = snapshot.desktop_size {
            send_server_event(
                ws_tx,
                ServerEvent::Status(ServerStatus::DesktopSize { width, height }),
            )
            .await?;
        }
    } else if let Some(reason) = &snapshot.disconnected {
        send_server_event(
            ws_tx,
            ServerEvent::Status(ServerStatus::Disconnected {
                reason: reason.clone(),
            }),
        )
        .await?;
    } else if snapshot.connecting {
        send_server_event(ws_tx, ServerEvent::Status(ServerStatus::Connecting)).await?;
    }

    Ok(())
}

async fn send_server_event(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    event: ServerEvent,
) -> anyhow::Result<()> {
    match event {
        ServerEvent::Status(status) => {
            let payload = serde_json::to_string(&status)?;
            ws_tx.send(Message::Text(payload.into())).await?;
        }
        ServerEvent::Video(frame) => {
            trace!(
                key = frame.key,
                bytes = frame.data.len(),
                "sending video frame to websocket"
            );
            ws_tx
                .send(Message::Binary(encode_video_frame(&frame).into()))
                .await?;
        }
    }

    Ok(())
}

pub async fn readiness_probe(config: &AppConfig) -> anyhow::Result<()> {
    tokio::net::TcpStream::connect((config.rdp.host.as_str(), config.rdp.port))
        .await
        .with_context(|| format!("connect TCP to {}:{}", config.rdp.host, config.rdp.port))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RdpConfig;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_config() -> AppConfig {
        AppConfig {
            ui_bind: "127.0.0.1:0".parse().unwrap(),
            tls: crate::config::TlsConfig {
                cert_path: "test.crt".into(),
                key_path: "test.key".into(),
            },
            rdp: RdpConfig {
                host: "127.0.0.1".to_owned(),
                port: 3389,
                username: "user".to_owned(),
                password: "secret".to_owned(),
                domain: None,
                mode: crate::config::RdpMode::Shared,
                routing_token: None,
                redirection_auth: None,
                width: 800,
                height: 600,
            },
        }
    }

    #[tokio::test]
    async fn health_endpoint_works() {
        let response = app(test_config())
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn public_config_endpoint_does_not_leak_secret() {
        let response = app(test_config())
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(body.contains("800"));
        assert!(body.contains("600"));
        assert!(!body.contains("127.0.0.1"));
        assert!(!body.contains("3389"));
        assert!(!body.contains("secret"));
        assert!(!body.contains("user"));
    }

    #[test]
    fn page_auto_connects_without_exposing_manual_controls() {
        let html = include_str!("../static/index.html");

        assert!(html.contains(r#"<div id="fps" class="metrics">0 fps</div>"#));
        assert!(html.contains("function recordFrameDrawn()"));
        assert!(html.contains("function renderFps()"));
        assert!(html.contains("window.setInterval(renderFps, 1000)"));
        assert!(html.contains(r#".shell.connected header:hover"#));
        assert!(html.contains(r#".shell.has-error header"#));
        assert!(html.contains(r#"shellEl.classList.toggle("connected", value)"#));
        assert!(html.contains(r#"shellEl.classList.toggle("has-error", kind === "error")"#));
        assert!(html.contains(r#"message.type === "error""#));
        assert!(html.contains("setReady(false);"));
        assert!(html.contains("new VideoDecoder"));
        assert!(html.contains("new EncodedVideoChunk"));
        assert!(html.contains("function h264CodecFromAnnexB(data)"));
        assert!(html.contains(r#"avc: { format: "annexb" }"#));
        assert!(html.contains("ctx.drawImage(frame, 0, 0)"));
        assert!(!html.contains("decodeQueueSize"));
        assert!(!html.contains("putImageData"));
        assert!(!html.contains("R2W3"));
        assert!(!html.contains("42E01F"));
        assert!(html.contains(r#"<div id="resolution" class="size-controls" hidden>"#));
        assert!(html.contains(r#"<select id="resolutionPreset""#));
        assert!(html.contains(r#"<option value="auto">Auto</option>"#));
        assert!(html.contains(r#"id="fullscreenToggle""#));
        assert!(html.contains("requestFullscreen()"));
        assert!(html.contains("document.exitFullscreen()"));
        assert!(html.contains(r#""fullscreenchange""#));
        assert!(!html.contains(r#"<option value="custom">Custom</option>"#));
        assert!(!html.contains(r#"type="number""#));
        assert!(html.contains("function applyAutoResize()"));
        assert!(html.contains(r#"const resolutionStorageKey = "rdp2web.resolutionPreset";"#));
        assert!(html.contains("function loadResolutionPreset()"));
        assert!(html.contains("function saveResolutionPreset()"));
        assert!(html.contains("window.localStorage.getItem(resolutionStorageKey)"));
        assert!(html.contains("window.localStorage.setItem(resolutionStorageKey, resolutionPreset.value)"));
        assert!(html.contains("loadResolutionPreset();"));
        assert!(html.contains("stageEl.clientWidth || document.documentElement.clientWidth"));
        assert!(html.contains("stageEl.clientHeight || document.documentElement.clientHeight"));
        assert!(html.contains(r#"send({ type: "resize""#));
        assert!(html.contains("max-height: 100%;"));
        assert!(html.contains("max-width: 100%;"));
        assert!(html.contains("function fitCanvas()"));
        assert!(html.contains("window.devicePixelRatio"));
        assert!(html.contains("1 / dpr"));
        assert!(html.contains(r#"wsUrl.protocol = wsUrl.protocol === "https:" ? "wss:" : "ws:";"#));
        assert!(html.contains("new WebSocket(wsUrl)"));
        assert!(!html.contains("Connect</button>"));
        assert!(!html.contains("Disconnect</button>"));
        assert!(!html.contains("rdp_host"));
        assert!(!html.contains("rdp_port"));
    }

    #[test]
    fn rdp_query_overrides_session_size() {
        let base = test_config().rdp;
        let query = RdpQuery {
            width: Some(2560),
            height: Some(1440),
        };

        let config = rdp_config_with_query(&base, &query).unwrap();

        assert_eq!(config.width, 2560);
        assert_eq!(config.height, 1440);
        assert_eq!(config.username, "user");
        assert_eq!(config.password, "secret");
        assert_eq!(config.mode, crate::config::RdpMode::Shared);
    }

    #[test]
    fn saved_session_snapshot_does_not_replay_video_history() {
        let mut snapshot = RdpSessionSnapshot::default();

        update_saved_snapshot(
            &mut snapshot,
            &ServerEvent::Video(crate::rdp::VideoFrame {
                key: false,
                data: vec![1],
            }),
        );

        update_saved_snapshot(
            &mut snapshot,
            &ServerEvent::Video(crate::rdp::VideoFrame {
                key: true,
                data: vec![2, 3],
            }),
        );
        update_saved_snapshot(
            &mut snapshot,
            &ServerEvent::Video(crate::rdp::VideoFrame {
                key: false,
                data: vec![4],
            }),
        );

        assert!(!snapshot.connected);
        assert!(snapshot.desktop_size.is_none());
        assert!(snapshot.error.is_none());
    }

    #[test]
    fn reconnect_nudge_keeps_rdp_width_even() {
        assert_eq!(nudge_width(3840), 3838);
        assert_eq!(nudge_width(200), 202);
    }
}
