use crate::config::RdpConfig;
use crate::input::slow_input_events_from_client;
use anyhow::{Context, bail};
use ironrdp::connector;
use ironrdp::connector::{ConnectionResult, Credentials};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp_core::WriteBuf;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_egfx::client::{EncodedAvc420Frame, GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::pdu::{CapabilitiesV81Flags, CapabilitySet, Codec2Type, WireToSurface2Pdu};
use ironrdp_pdu::rdp::capability_sets::client_codecs_capabilities;
use ironrdp_pdu::rdp::client_info::{CompressionType, PerformanceFlags, TimezoneInfo};
use ironrdp_pdu::rdp::headers::ShareDataPdu;
use serde::{Deserialize, Serialize};
use sspi::network_client::reqwest_network_client::ReqwestNetworkClient;
use std::io::Write as _;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::TryRecvError;
use std::sync::{Once, mpsc};
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_rustls::rustls;
use tracing::{debug, error, info, trace, warn};

const CONNECT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_millis(5);
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    Pointer {
        action: PointerAction,
        x: u16,
        y: u16,
        button: Option<u8>,
        delta_y: Option<i16>,
    },
    Key {
        down: bool,
        code: String,
        key: Option<String>,
    },
    Resize {
        width: u16,
        height: u16,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerAction {
    Move,
    Down,
    Up,
    Wheel,
}

#[derive(Debug)]
pub enum ServerEvent {
    Status(ServerStatus),
    Video(VideoFrame),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub key: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerStatus {
    Connecting,
    Connected,
    DesktopSize { width: u32, height: u32 },
    Disconnected { reason: String },
    Error { message: String },
}

pub fn encode_video_frame(frame: &VideoFrame) -> Vec<u8> {
    let mut packet = Vec::with_capacity(frame.data.len() + 2);
    packet.push(1);
    packet.push(u8::from(frame.key));
    packet.extend_from_slice(&frame.data);
    packet
}

fn avc_to_video_frame(data: &[u8]) -> anyhow::Result<VideoFrame> {
    if is_annex_b_stream(data) {
        return annex_b_to_video_frame(data);
    }

    let mut annex_b = Vec::with_capacity(data.len() + 16);
    let mut key = false;
    let mut offset = 0;

    while offset + 4 <= data.len() {
        let nal_len = usize::try_from(u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]))
        .expect("u32 NAL length fits usize");
        offset += 4;

        let end = offset
            .checked_add(nal_len)
            .context("AVC NAL length overflow")?;
        if nal_len == 0 || end > data.len() {
            bail!("AVC packet contains an invalid NAL length");
        }

        let nal = &data[offset..end];
        let nal_type = nal[0] & 0x1f;
        key |= nal_type == 5;
        annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        annex_b.extend_from_slice(nal);
        offset = end;
    }

    if offset != data.len() {
        bail!("AVC packet has trailing bytes");
    }
    if annex_b.is_empty() {
        bail!("AVC packet contained no NAL units");
    }

    Ok(VideoFrame { key, data: annex_b })
}

fn annex_b_to_video_frame(data: &[u8]) -> anyhow::Result<VideoFrame> {
    let mut normalized = Vec::with_capacity(data.len());
    let mut key = false;
    let mut offset = 0;

    while let Some((start_code, start_code_len)) = find_annex_b_start_code(data, offset) {
        let nal_start = start_code + start_code_len;
        let nal_end = find_annex_b_start_code(data, nal_start)
            .map_or(data.len(), |(next_start_code, _)| next_start_code);
        let nal = &data[nal_start..nal_end];
        if !nal.is_empty() {
            key |= (nal[0] & 0x1f) == 5;
            normalized.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            normalized.extend_from_slice(nal);
        }
        offset = nal_end;
    }

    if normalized.is_empty() {
        bail!("Annex B H.264 packet contained no NAL units");
    }

    Ok(VideoFrame {
        key,
        data: normalized,
    })
}

fn is_annex_b_stream(data: &[u8]) -> bool {
    find_annex_b_start_code(data, 0).is_some_and(|(start, _)| data[..start].iter().all(|b| *b == 0))
}

fn find_annex_b_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut offset = from;
    while offset + 3 <= data.len() {
        if offset + 4 <= data.len() && data[offset..offset + 4] == [0, 0, 0, 1] {
            return Some((offset, 4));
        }
        if data[offset..offset + 3] == [0, 0, 1] {
            return Some((offset, 3));
        }
        offset += 1;
    }
    None
}

struct EgfxHandler {
    server_tx: tokio_mpsc::Sender<ServerEvent>,
    progressive_updates_seen: u64,
}

impl EgfxHandler {
    fn new(server_tx: tokio_mpsc::Sender<ServerEvent>) -> Self {
        Self {
            server_tx,
            progressive_updates_seen: 0,
        }
    }
}

impl GraphicsPipelineHandler for EgfxHandler {
    fn capabilities(&self) -> Vec<CapabilitySet> {
        vec![CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
        }]
    }

    fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
        info!(?caps, "EGFX capabilities confirmed");
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        if self
            .server_tx
            .blocking_send(ServerEvent::Status(ServerStatus::DesktopSize {
                width,
                height,
            }))
            .is_err()
        {
            debug!("websocket receiver closed while sending desktop size update");
        }
    }

    fn on_avc420_frame(&mut self, frame: &EncodedAvc420Frame<'_>) {
        match avc_to_video_frame(frame.data) {
            Ok(video_frame) => {
                if self
                    .server_tx
                    .blocking_send(ServerEvent::Video(video_frame))
                    .is_err()
                {
                    debug!("websocket receiver closed while sending AVC420 frame");
                }
            }
            Err(err) => {
                warn!(
                    %err,
                    surface_id = frame.surface_id,
                    left = frame.destination_rectangle.left,
                    top = frame.destination_rectangle.top,
                    right = frame.destination_rectangle.right,
                    bottom = frame.destination_rectangle.bottom,
                    regions = frame.regions.len(),
                    "dropping invalid AVC420 frame"
                );
            }
        }
    }

    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        if pdu.codec_id != Codec2Type::RemoteFxProgressive {
            return;
        }

        self.progressive_updates_seen = self.progressive_updates_seen.saturating_add(1);
        if self.progressive_updates_seen == 1 {
            let _ = self
                .server_tx
                .blocking_send(ServerEvent::Status(ServerStatus::Error {
                    message:
                        "RDP server negotiated RemoteFX Progressive; direct AVC/H.264 is required"
                            .to_owned(),
                }));
            warn!(
                surface_id = pdu.surface_id,
                codec_context_id = pdu.codec_context_id,
                updates = self.progressive_updates_seen,
                "RDP server negotiated unsupported RemoteFX Progressive"
            );
        }
    }
}

#[cfg(test)]
const RFX_PROGRESSIVE_SYNC_BLOCK: u16 = 0xCCC0;
#[cfg(test)]
const RFX_PROGRESSIVE_CONTEXT_BLOCK: u16 = 0xCCC3;
#[cfg(test)]
const RFX_PROGRESSIVE_BLOCK_HEADER_SIZE: usize = 6;

#[cfg(test)]
fn progressive_bitmap_with_context<'a>(
    bitmap_data: &'a [u8],
    context_prefix: Option<&[u8]>,
) -> std::borrow::Cow<'a, [u8]> {
    if progressive_has_context(bitmap_data) {
        return std::borrow::Cow::Borrowed(bitmap_data);
    }

    let Some(context_prefix) = context_prefix else {
        return std::borrow::Cow::Borrowed(bitmap_data);
    };

    let mut data = Vec::with_capacity(context_prefix.len() + bitmap_data.len());
    data.extend_from_slice(context_prefix);
    data.extend_from_slice(bitmap_data);
    std::borrow::Cow::Owned(data)
}

#[cfg(test)]
fn progressive_has_context(bitmap_data: &[u8]) -> bool {
    progressive_context_prefix(bitmap_data).is_some()
}

#[cfg(test)]
fn progressive_context_prefix(bitmap_data: &[u8]) -> Option<Vec<u8>> {
    let mut offset = 0usize;
    let mut prefix = Vec::new();
    let mut has_context = false;

    while offset + RFX_PROGRESSIVE_BLOCK_HEADER_SIZE <= bitmap_data.len() {
        let block_type = u16::from_le_bytes([bitmap_data[offset], bitmap_data[offset + 1]]);
        let block_len = u32::from_le_bytes([
            bitmap_data[offset + 2],
            bitmap_data[offset + 3],
            bitmap_data[offset + 4],
            bitmap_data[offset + 5],
        ]);
        let Ok(block_len) = usize::try_from(block_len) else {
            break;
        };
        let Some(block_end) = offset.checked_add(block_len) else {
            break;
        };
        if block_len < RFX_PROGRESSIVE_BLOCK_HEADER_SIZE || block_end > bitmap_data.len() {
            break;
        }

        if matches!(
            block_type,
            RFX_PROGRESSIVE_SYNC_BLOCK | RFX_PROGRESSIVE_CONTEXT_BLOCK
        ) {
            prefix.extend_from_slice(&bitmap_data[offset..block_end]);
            has_context |= block_type == RFX_PROGRESSIVE_CONTEXT_BLOCK;
        } else if has_context {
            break;
        }

        offset = block_end;
    }

    has_context.then_some(prefix)
}

pub fn start_rdp_session(
    config: RdpConfig,
) -> (mpsc::Sender<ClientEvent>, tokio_mpsc::Receiver<ServerEvent>) {
    let (control_tx, control_rx) = mpsc::channel();
    let (server_tx, server_rx) = tokio_mpsc::channel(32);

    std::thread::spawn(move || {
        let result = run_rdp_session(config, control_rx, server_tx.clone());
        if let Err(err) = result {
            error!(%err, "RDP session failed");
            let _ = server_tx.blocking_send(ServerEvent::Status(ServerStatus::Error {
                message: err.to_string(),
            }));
        }
    });

    (control_tx, server_rx)
}

pub fn validate_rdp_login(config: RdpConfig) -> anyhow::Result<(u16, u16)> {
    let connector_config = build_connector_config(&config)?;
    let (server_tx, _server_rx) = tokio_mpsc::channel(1);
    let (connection_result, _framed) = connect(
        connector_config,
        config.host.clone(),
        config.port,
        server_tx,
    )
    .context("connect RDP")?;
    Ok((
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
    ))
}

fn run_rdp_session(
    config: RdpConfig,
    control_rx: mpsc::Receiver<ClientEvent>,
    server_tx: tokio_mpsc::Sender<ServerEvent>,
) -> anyhow::Result<()> {
    server_tx
        .blocking_send(ServerEvent::Status(ServerStatus::Connecting))
        .ok();

    let connector_config = build_connector_config(&config)?;
    let (connection_result, framed) = connect(
        connector_config,
        config.host.clone(),
        config.port,
        server_tx.clone(),
    )
    .context("connect RDP")?;
    let width = connection_result.desktop_size.width;
    let height = connection_result.desktop_size.height;

    server_tx
        .blocking_send(ServerEvent::Status(ServerStatus::Connected))
        .ok();

    let image = DecodedImage::new(
        ironrdp_graphics::image_processing::PixelFormat::RgbA32,
        width,
        height,
    );

    drive_active_session(connection_result, framed, image, control_rx)
}

fn build_connector_config(config: &RdpConfig) -> anyhow::Result<connector::Config> {
    let codecs = client_codecs_capabilities(&["remotefx"]).map_err(|err| anyhow::anyhow!(err))?;

    Ok(connector::Config {
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        enable_tls: false,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize {
            width: config.width,
            height: config.height,
        },
        bitmap: Some(connector::BitmapConfig {
            lossy_compression: false,
            color_depth: 32,
            codecs,
        }),
        client_build: 0,
        client_name: "rdp2web".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(target_os = "ios")]
        platform: MajorPlatformType::IOS,
        #[cfg(target_os = "linux")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "android")]
        platform: MajorPlatformType::ANDROID,
        #[cfg(target_os = "freebsd")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "dragonfly")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "openbsd")]
        platform: MajorPlatformType::UNIX,
        #[cfg(target_os = "netbsd")]
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: true,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        compression_type: Some(CompressionType::Rdp61),
        pointer_software_rendering: false,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    })
}

type UpgradedFramed =
    ironrdp_blocking::Framed<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

fn connect(
    config: connector::Config,
    server_name: String,
    port: u16,
    server_tx: tokio_mpsc::Sender<ServerEvent>,
) -> anyhow::Result<(ConnectionResult, UpgradedFramed)> {
    install_default_crypto_provider();

    let server_addr = (server_name.as_str(), port)
        .to_socket_addrs()
        .context("resolve RDP host")?
        .next()
        .context("RDP host did not resolve to any socket address")?;
    info!(%server_addr, "connecting to RDP endpoint");

    let tcp_stream = TcpStream::connect(server_addr).context("TCP connect")?;
    tcp_stream
        .set_read_timeout(Some(CONNECT_READ_TIMEOUT))
        .context("set RDP connect read timeout")?;

    let client_addr = tcp_stream.local_addr().context("get TCP local address")?;
    let mut framed = ironrdp_blocking::Framed::new(tcp_stream);
    let graphics = GraphicsPipelineClient::new(Box::new(EgfxHandler::new(server_tx)), None);
    let drdynvc = DrdynvcClient::new().with_dynamic_channel(graphics);
    let mut connector =
        connector::ClientConnector::new(config, client_addr).with_static_channel(drdynvc);
    let should_upgrade = ironrdp_blocking::connect_begin(&mut framed, &mut connector)
        .context("begin RDP connect")?;

    let initial_stream = framed.into_inner_no_leftover();
    let (upgraded_stream, server_public_key) =
        tls_upgrade(initial_stream, server_name.clone()).context("TLS upgrade")?;
    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut connector);
    let mut upgraded_framed = ironrdp_blocking::Framed::new(upgraded_stream);
    let mut network_client = ReqwestNetworkClient;
    let connection_result = ironrdp_blocking::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut network_client,
        server_name.into(),
        server_public_key,
        None,
    )
    .context("finalize RDP connect")?;

    let (stream, _) = upgraded_framed.get_inner_mut();
    stream
        .sock
        .set_read_timeout(Some(READ_TIMEOUT))
        .context("set RDP active-session read timeout")?;

    Ok((connection_result, upgraded_framed))
}

fn install_default_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn drive_active_session(
    connection_result: ConnectionResult,
    mut framed: UpgradedFramed,
    mut image: DecodedImage,
    control_rx: mpsc::Receiver<ClientEvent>,
) -> anyhow::Result<()> {
    let mut active_stage = ActiveStage::new(connection_result);

    loop {
        loop {
            match control_rx.try_recv() {
                Ok(event) => match event {
                    ClientEvent::Resize { width, height } => {
                        if let Some(result) =
                            active_stage.encode_resize(width.into(), height.into(), None, None)
                        {
                            let frame = result.context("encode RDP resize")?;
                            framed.write_all(&frame).context("write resize frame")?;
                            flush_framed(&mut framed).context("flush resize frame")?;
                        }
                    }
                    event => {
                        let events = slow_input_events_from_client(&event);
                        if events.is_empty() {
                            continue;
                        }
                        let mut frame = WriteBuf::new();
                        active_stage
                            .encode_static(
                                &mut frame,
                                ShareDataPdu::Input(ironrdp_pdu::input::InputEventPdu(events)),
                            )
                            .context("encode RDP input")?;
                        framed
                            .write_all(frame.filled())
                            .context("write RDP input frame")?;
                        flush_framed(&mut framed).context("flush RDP input frame")?;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    debug!("websocket control channel closed; stopping RDP session");
                    return Ok(());
                }
            }
        }

        match framed.read_pdu() {
            Ok((action, payload)) => {
                trace!(?action, frame_length = payload.len(), "RDP frame received");
                let outputs = active_stage
                    .process(&mut image, action, &payload)
                    .context("process RDP frame")?;
                if !write_outputs(outputs, &mut framed)? {
                    debug!("websocket receiver closed; stopping RDP session");
                    return Ok(());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(err) => bail!("read RDP frame: {err}"),
        }
    }
}

fn write_outputs(
    outputs: Vec<ActiveStageOutput>,
    framed: &mut UpgradedFramed,
) -> anyhow::Result<bool> {
    for output in outputs {
        match output {
            ActiveStageOutput::ResponseFrame(frame) => {
                framed.write_all(&frame).context("write RDP response")?;
                flush_framed(framed).context("flush RDP response")?;
            }
            ActiveStageOutput::GraphicsUpdate(rect) => {
                trace!(?rect, "ignoring classic bitmap update in direct video mode");
            }
            ActiveStageOutput::Terminate(reason) => {
                bail!("RDP session terminated: {reason:?}");
            }
            _ => {}
        }
    }
    Ok(true)
}

fn flush_framed(framed: &mut UpgradedFramed) -> std::io::Result<()> {
    let (stream, _) = framed.get_inner_mut();
    stream.flush()
}

fn tls_upgrade(
    stream: TcpStream,
    server_name: String,
) -> anyhow::Result<(
    rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    Vec<u8>,
)> {
    let mut config = rustls::client::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(danger::NoCertificateVerification))
        .with_no_client_auth();
    config.resumption = rustls::client::Resumption::disabled();
    let config = std::sync::Arc::new(config);
    let server_name = server_name.try_into()?;
    let client = rustls::ClientConnection::new(config, server_name)?;
    let mut tls_stream = rustls::StreamOwned::new(client, stream);
    tls_stream.flush().context("flush TLS handshake")?;

    let cert = tls_stream
        .conn
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .context("RDP server TLS certificate is missing")?;
    let server_public_key = extract_tls_server_public_key(cert.as_ref())?;
    Ok((tls_stream, server_public_key))
}

fn extract_tls_server_public_key(cert: &[u8]) -> anyhow::Result<Vec<u8>> {
    use x509_cert::der::Decode as _;
    let cert = x509_cert::Certificate::from_der(cert)?;
    let server_public_key = cert
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .context("subject public key BIT STRING is not aligned")?
        .to_owned();
    Ok(server_public_key)
}

mod danger {
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::{DigitallySignedStruct, Error, SignatureScheme, pki_types};

    #[derive(Debug)]
    pub(super) struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        // GNOME Remote Desktop uses a self-signed RDP TLS certificate. IronRDP's
        // CredSSP finalization receives the extracted TLS server public key
        // from `tls_upgrade`, so certificate chain validation is deliberately
        // bypassed here while still binding CredSSP to the observed RDP TLS key.
        fn verify_server_cert(
            &self,
            _: &pki_types::CertificateDer<'_>,
            _: &[pki_types::CertificateDer<'_>],
            _: &pki_types::ServerName<'_>,
            _: &[u8],
            _: pki_types::UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA1,
                SignatureScheme::ECDSA_SHA1_Legacy,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::ED448,
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_event_deserializes() {
        let event: ClientEvent = serde_json::from_str(
            r#"{"type":"pointer","action":"move","x":12,"y":20,"button":null}"#,
        )
        .unwrap();

        assert!(matches!(
            event,
            ClientEvent::Pointer {
                action: PointerAction::Move,
                x: 12,
                y: 20,
                ..
            }
        ));
    }

    #[test]
    fn egfx_advertises_only_avc420_capability() {
        let (server_tx, _server_rx) = tokio_mpsc::channel(1);
        let handler = EgfxHandler::new(server_tx);
        let capabilities = handler.capabilities();

        assert_eq!(capabilities.len(), 1);
        let CapabilitySet::V8_1 { flags } = capabilities[0] else {
            panic!("rdp2web must not advertise AVC444-capable EGFX versions");
        };
        assert!(flags.contains(CapabilitiesV81Flags::AVC420_ENABLED));
        assert!(flags.contains(CapabilitiesV81Flags::SMALL_CACHE));
    }

    #[test]
    fn avc_video_frame_is_annex_b_and_marks_idr_as_key() {
        let sps = [0x67, 0x42, 0xe0, 0x1f];
        let idr = [0x65, 0x88, 0x84];
        let avc = [length_prefixed(&sps), length_prefixed(&idr)].concat();

        let frame = avc_to_video_frame(&avc).expect("convert AVC packet");

        assert!(frame.key);
        assert_eq!(
            frame.data,
            [&[0, 0, 0, 1][..], &sps[..], &[0, 0, 0, 1][..], &idr[..]].concat()
        );
    }

    #[test]
    fn annex_b_video_frame_is_accepted_and_marks_idr_as_key() {
        let sps = [0x67, 0x42, 0xe0, 0x1f];
        let idr = [0x65, 0x88, 0x84];
        let annex_b = [&[0, 0, 1][..], &sps[..], &[0, 0, 0, 1][..], &idr[..]].concat();

        let frame = avc_to_video_frame(&annex_b).expect("convert Annex B packet");

        assert!(frame.key);
        assert_eq!(
            frame.data,
            [&[0, 0, 0, 1][..], &sps[..], &[0, 0, 0, 1][..], &idr[..]].concat()
        );
    }

    #[test]
    fn direct_video_wire_format_uses_binary_h264_header() {
        let packet = encode_video_frame(&VideoFrame {
            key: true,
            data: vec![0, 0, 0, 1, 0x65],
        });

        assert_eq!(&packet[..2], &[1, 1]);
        assert_eq!(&packet[2..], &[0, 0, 0, 1, 0x65]);
    }

    #[test]
    fn progressive_context_prefix_is_reused_for_incremental_streams() {
        let first = [
            block(RFX_PROGRESSIVE_SYNC_BLOCK, &[1, 2, 3, 4, 5, 6]),
            block(RFX_PROGRESSIVE_CONTEXT_BLOCK, &[7, 8, 9, 10]),
            block(0xCCC1, &[11, 12, 13, 14, 15, 16]),
        ]
        .concat();
        let incremental = block(0xCCC1, &[21, 22, 23, 24, 25, 26]);
        let prefix = progressive_context_prefix(&first).expect("context prefix");

        let repaired = progressive_bitmap_with_context(&incremental, Some(&prefix));

        assert_eq!(prefix.len(), 22);
        assert!(progressive_has_context(&first));
        assert!(!progressive_has_context(&incremental));
        assert_eq!(&repaired[..prefix.len()], prefix.as_slice());
        assert_eq!(&repaired[prefix.len()..], incremental.as_slice());
    }

    fn block(block_type: u16, body: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(RFX_PROGRESSIVE_BLOCK_HEADER_SIZE + body.len());
        data.extend_from_slice(&block_type.to_le_bytes());
        data.extend_from_slice(
            &u32::try_from(RFX_PROGRESSIVE_BLOCK_HEADER_SIZE + body.len())
                .unwrap()
                .to_le_bytes(),
        );
        data.extend_from_slice(body);
        data
    }

    fn length_prefixed(nal: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(nal.len() + 4);
        data.extend_from_slice(&u32::try_from(nal.len()).unwrap().to_be_bytes());
        data.extend_from_slice(nal);
        data
    }
}
