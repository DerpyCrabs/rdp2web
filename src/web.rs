use crate::config::AppConfig;
use crate::rdp::{ClientEvent, ServerEvent, encode_video_frame, start_rdp_session};
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
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{self, ServerConfig};
use tokio_rustls::server::TlsStream;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};

const DIRECT_VIDEO_MEDIA: &str = r#"{"type":"media","video":{"h264":{"format":"annexb"}}}"#;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
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
        .on_upgrade(move |socket| handle_rdp_socket(socket, rdp_config))
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

async fn handle_rdp_socket(socket: WebSocket, rdp_config: crate::config::RdpConfig) {
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
        assert!(html.contains(r#"shellEl.classList.toggle("connected", value)"#));
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
        assert!(html.contains("max-height: 100%;"));
        assert!(html.contains("max-width: 100%;"));
        assert!(html.contains("function fitCanvas()"));
        assert!(html.contains("window.devicePixelRatio"));
        assert!(html.contains("1 / dpr"));
        assert!(html.contains(r#"wsUrl.protocol = wsUrl.protocol === "https:" ? "wss:" : "ws:";"#));
        assert!(html.contains("new WebSocket(wsUrl)"));
        assert!(!html.contains(r#"type: "resize""#));
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
    }
}
