use crate::config::{RdpConfig, RdpMode, RdpRedirectionAuth};
use crate::input::slow_input_events_from_client;
use anyhow::{Context, bail};
use ironrdp::connector;
use ironrdp::connector::{ConnectionResult, Credentials};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp_core::WriteBuf;
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_egfx::client::{EncodedAvc420Frame, GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::pdu::{CapabilitiesV81Flags, CapabilitySet, Codec2Type, WireToSurface2Pdu};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::rdp::capability_sets::client_codecs_capabilities;
use ironrdp_pdu::rdp::client_info::{CompressionType, PerformanceFlags, TimezoneInfo};
use ironrdp_pdu::rdp::headers::ShareDataPdu;
use ironrdp_pdu::rdp::refresh_rectangle::RefreshRectanglePdu;
use ironrdp_pdu::rdp::suppress_output::SuppressOutputPdu;
use serde::{Deserialize, Serialize};
use sspi::network_client::reqwest_network_client::ReqwestNetworkClient;
use std::io::{Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::TryRecvError;
use std::sync::{Once, mpsc};
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_rustls::rustls;
use tracing::{debug, error, info, trace, warn};

const SHARED_CONNECT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_CONNECT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_millis(5);
const MAX_SERVER_REDIRECTS: usize = 4;
const RDSTLS_VERSION_1: u16 = 0x0001;
const RDSTLS_TYPE_CAPABILITIES: u16 = 0x0001;
const RDSTLS_TYPE_AUTHREQ: u16 = 0x0002;
const RDSTLS_TYPE_AUTHRSP: u16 = 0x0004;
const RDSTLS_DATA_CAPABILITIES: u16 = 0x0001;
const RDSTLS_DATA_PASSWORD_CREDS: u16 = 0x0001;
const RDSTLS_DATA_RESULT_CODE: u16 = 0x0001;
const RDSTLS_RESULT_SUCCESS: u32 = 0x0000_0000;
const RDSTLS_RESULT_ACCESS_DENIED: u32 = 0x0000_0005;
const RDSTLS_RESULT_LOGON_FAILURE: u32 = 0x0000_052e;
const RDSTLS_RESULT_INVALID_LOGON_HOURS: u32 = 0x0000_0530;
const RDSTLS_RESULT_PASSWORD_EXPIRED: u32 = 0x0000_0532;
const RDSTLS_RESULT_ACCOUNT_DISABLED: u32 = 0x0000_0533;
const RDSTLS_RESULT_PASSWORD_MUST_CHANGE: u32 = 0x0000_0773;
const RDSTLS_RESULT_ACCOUNT_LOCKED_OUT: u32 = 0x0000_0775;
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
    Refresh {
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

#[derive(Debug, Clone)]
pub enum ServerEvent {
    Status(ServerStatus),
    Video(VideoFrame),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub key: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
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
                let key = video_frame.key;
                let bytes = video_frame.data.len();
                if self
                    .server_tx
                    .blocking_send(ServerEvent::Video(video_frame))
                    .is_err()
                {
                    debug!("websocket receiver closed while sending AVC420 frame");
                } else {
                    trace!(
                        surface_id = frame.surface_id,
                        key, bytes, "forwarded AVC420 frame to server event channel"
                    );
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
                message: format!("{err:#}"),
            }));
        }
    });

    (control_tx, server_rx)
}

pub fn validate_rdp_login(config: RdpConfig) -> anyhow::Result<(u16, u16)> {
    let connect_read_timeout = connect_read_timeout_for_mode(config.mode);
    let connector_config = build_connector_config(&config)?;
    let (server_tx, _server_rx) = tokio_mpsc::channel(1);
    let (connection_result, _framed) = connect(
        connector_config,
        &config,
        config.host.clone(),
        config.port,
        server_tx,
        connect_read_timeout,
    )
    .context("connect RDP")?;
    Ok((
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
    ))
}

fn run_rdp_session(
    mut config: RdpConfig,
    control_rx: mpsc::Receiver<ClientEvent>,
    server_tx: tokio_mpsc::Sender<ServerEvent>,
) -> anyhow::Result<()> {
    for redirect_count in 0..=MAX_SERVER_REDIRECTS {
        server_tx
            .blocking_send(ServerEvent::Status(ServerStatus::Connecting))
            .ok();

        let connector_config = build_connector_config(&config)?;
        let connect_read_timeout = connect_read_timeout_for_mode(config.mode);
        let (connection_result, framed) = connect(
            connector_config,
            &config,
            config.host.clone(),
            config.port,
            server_tx.clone(),
            connect_read_timeout,
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

        match drive_active_session(connection_result, framed, image, &control_rx)? {
            ActiveSessionOutcome::Closed => return Ok(()),
            ActiveSessionOutcome::Redirect(redirection) => {
                if redirect_count == MAX_SERVER_REDIRECTS {
                    bail!("RDP server redirected too many times");
                }
                info!(
                    redirect = redirect_count + 1,
                    has_username = redirection.username.is_some(),
                    has_password = redirection.password.is_some(),
                    has_domain = redirection.domain.is_some(),
                    "following RDP server redirection"
                );
                apply_server_redirection(&mut config, redirection);
            }
        }
    }

    bail!("RDP server redirected too many times")
}

fn build_connector_config(config: &RdpConfig) -> anyhow::Result<connector::Config> {
    let codecs = client_codecs_capabilities(&["remotefx"]).map_err(|err| anyhow::anyhow!(err))?;
    let is_session_mode = config.mode == RdpMode::Session;

    Ok(connector::Config {
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        request_data: config
            .routing_token
            .as_ref()
            .map(|token| ironrdp_pdu::nego::NegoRequestData::routing_token(token.clone())),
        enable_tls: is_session_mode,
        enable_credssp: true,
        enable_rdstls: is_session_mode && config.routing_token.is_some(),
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
        platform: platform_for_mode(config.mode),
        enable_server_pointer: true,
        autologon: is_session_mode,
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

fn platform_for_mode(mode: RdpMode) -> MajorPlatformType {
    if mode == RdpMode::Session {
        // GNOME Remote Login hands non-RDSTLS Windows clients back to the
        // original system credentials during server redirection.
        return MajorPlatformType::WINDOWS;
    }

    default_platform()
}

#[cfg(windows)]
fn default_platform() -> MajorPlatformType {
    MajorPlatformType::WINDOWS
}

#[cfg(target_os = "macos")]
fn default_platform() -> MajorPlatformType {
    MajorPlatformType::MACINTOSH
}

#[cfg(target_os = "ios")]
fn default_platform() -> MajorPlatformType {
    MajorPlatformType::IOS
}

#[cfg(target_os = "linux")]
fn default_platform() -> MajorPlatformType {
    MajorPlatformType::UNIX
}

#[cfg(target_os = "android")]
fn default_platform() -> MajorPlatformType {
    MajorPlatformType::ANDROID
}

#[cfg(any(
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn default_platform() -> MajorPlatformType {
    MajorPlatformType::UNIX
}

fn connect_read_timeout_for_mode(mode: RdpMode) -> Duration {
    match mode {
        RdpMode::Shared => SHARED_CONNECT_READ_TIMEOUT,
        RdpMode::Session => SESSION_CONNECT_READ_TIMEOUT,
    }
}

type UpgradedStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
type UpgradedFramed = ironrdp_blocking::Framed<UpgradedStream>;

fn connect(
    config: connector::Config,
    rdp_config: &RdpConfig,
    server_name: String,
    port: u16,
    server_tx: tokio_mpsc::Sender<ServerEvent>,
    connect_read_timeout: Duration,
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
        .set_read_timeout(Some(connect_read_timeout))
        .context("set RDP connect read timeout")?;

    let client_addr = tcp_stream.local_addr().context("get TCP local address")?;
    let mut framed = ironrdp_blocking::Framed::new(tcp_stream);
    let graphics = GraphicsPipelineClient::new(Box::new(EgfxHandler::new(server_tx)), None);
    let display_control = DisplayControlClient::new(|_| Ok(Vec::new()));
    let drdynvc = DrdynvcClient::new()
        .with_dynamic_channel(graphics)
        .with_dynamic_channel(display_control);
    let mut connector =
        connector::ClientConnector::new(config, client_addr).with_static_channel(drdynvc);
    let should_upgrade = ironrdp_blocking::connect_begin(&mut framed, &mut connector)
        .context("begin RDP connect")?;

    let initial_stream = framed.into_inner_no_leftover();
    let (mut upgraded_stream, server_public_key) =
        tls_upgrade(initial_stream, server_name.clone()).context("TLS upgrade")?;
    if selected_protocol_from_connector(&connector)
        .is_some_and(|protocol| protocol.contains(ironrdp_pdu::nego::SecurityProtocol::RDSTLS))
    {
        perform_rdstls_authentication(&mut upgraded_stream, rdp_config)
            .context("RDSTLS authentication")?;
    }
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

fn selected_protocol_from_connector(
    connector: &connector::ClientConnector,
) -> Option<ironrdp_pdu::nego::SecurityProtocol> {
    match connector.state {
        connector::ClientConnectorState::EnhancedSecurityUpgrade { selected_protocol } => {
            Some(selected_protocol)
        }
        _ => None,
    }
}

fn perform_rdstls_authentication(
    stream: &mut UpgradedStream,
    config: &RdpConfig,
) -> anyhow::Result<()> {
    let auth = config
        .redirection_auth
        .as_ref()
        .context("RDP server selected RDSTLS without redirected authentication data")?;
    let password = auth
        .password
        .as_deref()
        .context("RDP server selected RDSTLS but did not provide a redirected password")?;

    read_rdstls_capabilities(stream)?;

    let username = auth.username.as_deref().unwrap_or(&config.username);
    let domain = auth.domain.as_deref().or(config.domain.as_deref());
    let redirection_guid = auth.redirection_guid.as_deref().unwrap_or(&[]);
    let request = rdstls_authentication_request(username, domain, redirection_guid, password)?;
    stream
        .write_all(&request)
        .context("write RDSTLS authentication request")?;
    stream
        .flush()
        .context("flush RDSTLS authentication request")?;

    read_rdstls_authentication_response(stream)
}

fn read_rdstls_capabilities(stream: &mut UpgradedStream) -> anyhow::Result<()> {
    let mut frame = [0u8; 8];
    stream
        .read_exact(&mut frame)
        .context("read RDSTLS capabilities")?;

    ensure_rdstls_header(&frame, RDSTLS_TYPE_CAPABILITIES)?;
    let data_type = u16::from_le_bytes([frame[4], frame[5]]);
    let supported_versions = u16::from_le_bytes([frame[6], frame[7]]);
    if data_type != RDSTLS_DATA_CAPABILITIES {
        bail!("RDSTLS capabilities used unsupported data type 0x{data_type:04x}");
    }
    if supported_versions & RDSTLS_VERSION_1 == 0 {
        bail!("RDSTLS server did not advertise version 1 support");
    }

    Ok(())
}

fn rdstls_authentication_request(
    username: &str,
    domain: Option<&str>,
    redirection_guid: &[u8],
    password: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let mut frame = Vec::new();
    push_u16_le(&mut frame, RDSTLS_VERSION_1);
    push_u16_le(&mut frame, RDSTLS_TYPE_AUTHREQ);
    push_u16_le(&mut frame, RDSTLS_DATA_PASSWORD_CREDS);
    push_len_prefixed_bytes(&mut frame, redirection_guid, "RDSTLS redirection GUID")?;
    push_len_prefixed_utf16(&mut frame, Some(username), "RDSTLS username")?;
    push_len_prefixed_utf16(&mut frame, domain, "RDSTLS domain")?;
    push_len_prefixed_bytes(&mut frame, password, "RDSTLS redirected password")?;
    Ok(frame)
}

fn read_rdstls_authentication_response(stream: &mut UpgradedStream) -> anyhow::Result<()> {
    let mut frame = [0u8; 10];
    stream
        .read_exact(&mut frame)
        .context("read RDSTLS authentication response")?;

    ensure_rdstls_header(&frame, RDSTLS_TYPE_AUTHRSP)?;
    let data_type = u16::from_le_bytes([frame[4], frame[5]]);
    if data_type != RDSTLS_DATA_RESULT_CODE {
        bail!("RDSTLS authentication response used unsupported data type 0x{data_type:04x}");
    }

    let result = u32::from_le_bytes([frame[6], frame[7], frame[8], frame[9]]);
    if result != RDSTLS_RESULT_SUCCESS {
        bail!(
            "RDSTLS authentication failed with {} (0x{result:08x})",
            rdstls_result_name(result)
        );
    }

    Ok(())
}

fn ensure_rdstls_header(frame: &[u8], expected_type: u16) -> anyhow::Result<()> {
    let version = u16::from_le_bytes([frame[0], frame[1]]);
    let pdu_type = u16::from_le_bytes([frame[2], frame[3]]);
    if version != RDSTLS_VERSION_1 {
        bail!("RDSTLS frame used unsupported version 0x{version:04x}");
    }
    if pdu_type != expected_type {
        bail!("RDSTLS frame used unexpected PDU type 0x{pdu_type:04x}");
    }
    Ok(())
}

fn push_len_prefixed_utf16(
    frame: &mut Vec<u8>,
    value: Option<&str>,
    field: &str,
) -> anyhow::Result<()> {
    let words = value
        .unwrap_or("")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let len = words
        .len()
        .checked_mul(2)
        .context("RDSTLS string length overflow")?;
    let len = u16::try_from(len).with_context(|| format!("{field} is too long"))?;
    push_u16_le(frame, len);
    for word in words {
        push_u16_le(frame, word);
    }
    Ok(())
}

fn push_len_prefixed_bytes(frame: &mut Vec<u8>, value: &[u8], field: &str) -> anyhow::Result<()> {
    let len = u16::try_from(value.len()).with_context(|| format!("{field} is too long"))?;
    push_u16_le(frame, len);
    frame.extend_from_slice(value);
    Ok(())
}

fn push_u16_le(frame: &mut Vec<u8>, value: u16) {
    frame.extend_from_slice(&value.to_le_bytes());
}

fn rdstls_result_name(result: u32) -> &'static str {
    match result {
        RDSTLS_RESULT_ACCESS_DENIED => "access denied",
        RDSTLS_RESULT_LOGON_FAILURE => "logon failure",
        RDSTLS_RESULT_INVALID_LOGON_HOURS => "invalid logon hours",
        RDSTLS_RESULT_PASSWORD_EXPIRED => "password expired",
        RDSTLS_RESULT_ACCOUNT_DISABLED => "account disabled",
        RDSTLS_RESULT_PASSWORD_MUST_CHANGE => "password must change",
        RDSTLS_RESULT_ACCOUNT_LOCKED_OUT => "account locked out",
        _ => "unknown error",
    }
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
    control_rx: &mpsc::Receiver<ClientEvent>,
) -> anyhow::Result<ActiveSessionOutcome> {
    let mut current_desktop_size = (
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
    );
    let mut active_stage = ActiveStage::new(connection_result);
    let mut pending_resize = None;

    loop {
        loop {
            match control_rx.try_recv() {
                Ok(event) => match event {
                    ClientEvent::Resize { width, height } => {
                        pending_resize = Some((width, height));
                    }
                    ClientEvent::Refresh { width, height } => {
                        write_static_pdu(
                            &active_stage,
                            &mut framed,
                            ShareDataPdu::SuppressOutput(SuppressOutputPdu {
                                desktop_rect: Some(full_desktop_rect(width, height)),
                            }),
                            "allow display updates",
                        )?;
                        write_static_pdu(
                            &active_stage,
                            &mut framed,
                            ShareDataPdu::RefreshRectangle(refresh_rectangle(width, height)),
                            "refresh rectangle",
                        )?;
                    }
                    event => {
                        let events = slow_input_events_from_client(&event);
                        if events.is_empty() {
                            continue;
                        }
                        write_static_pdu(
                            &active_stage,
                            &mut framed,
                            ShareDataPdu::Input(ironrdp_pdu::input::InputEventPdu(events)),
                            "input",
                        )?;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    debug!("websocket control channel closed; stopping RDP session");
                    return Ok(ActiveSessionOutcome::Closed);
                }
            }
        }

        if let Some((width, height)) = pending_resize {
            if current_desktop_size == (width, height) {
                pending_resize = None;
            } else if let Some(result) =
                active_stage.encode_resize(width.into(), height.into(), None, None)
            {
                let frame = result.context("encode RDP resize")?;
                framed.write_all(&frame).context("write resize frame")?;
                flush_framed(&mut framed).context("flush resize frame")?;
                current_desktop_size = (width, height);
                pending_resize = None;
            }
        }

        match framed.read_pdu() {
            Ok((action, payload)) => {
                trace!(?action, frame_length = payload.len(), "RDP frame received");
                if let Some(redirection) = server_redirection_from_frame(&payload)? {
                    return Ok(ActiveSessionOutcome::Redirect(redirection));
                }
                let outputs = active_stage
                    .process(&mut image, action, &payload)
                    .context("process RDP frame")?;
                if !write_outputs(outputs, &mut framed)? {
                    debug!("websocket receiver closed; stopping RDP session");
                    return Ok(ActiveSessionOutcome::Closed);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(err) => bail!("read RDP frame: {err}"),
        }
    }
}

fn refresh_rectangle(width: u16, height: u16) -> RefreshRectanglePdu {
    RefreshRectanglePdu {
        areas_to_refresh: vec![full_desktop_rect(width, height)],
    }
}

fn full_desktop_rect(width: u16, height: u16) -> InclusiveRectangle {
    InclusiveRectangle {
        left: 0,
        top: 0,
        right: width.saturating_sub(1),
        bottom: height.saturating_sub(1),
    }
}

fn write_static_pdu(
    active_stage: &ActiveStage,
    framed: &mut UpgradedFramed,
    pdu: ShareDataPdu,
    name: &'static str,
) -> anyhow::Result<()> {
    let mut frame = WriteBuf::new();
    active_stage
        .encode_static(&mut frame, pdu)
        .with_context(|| format!("encode RDP {name} frame"))?;
    framed
        .write_all(frame.filled())
        .with_context(|| format!("write RDP {name} frame"))?;
    flush_framed(framed).with_context(|| format!("flush RDP {name} frame"))?;
    Ok(())
}

enum ActiveSessionOutcome {
    Closed,
    Redirect(ServerRedirection),
}

#[derive(Debug)]
struct ServerRedirection {
    routing_token: String,
    username: Option<String>,
    password: Option<String>,
    domain: Option<String>,
    redirection_auth: Option<RdpRedirectionAuth>,
}

fn apply_server_redirection(config: &mut RdpConfig, redirection: ServerRedirection) {
    config.routing_token = Some(redirection.routing_token);

    if config.mode == RdpMode::Session {
        // Keep the configured login credentials for the normal Client Info
        // PDU, but use the raw redirected fields for RDSTLS handoff auth.
        config.redirection_auth = redirection.redirection_auth;
        return;
    }

    if let Some(username) = redirection.username {
        config.username = username;
    }
    if let Some(password) = redirection.password {
        config.password = password;
    }
    if redirection.domain.is_some() {
        config.domain = redirection.domain;
    }
}

fn server_redirection_from_frame(frame: &[u8]) -> anyhow::Result<Option<ServerRedirection>> {
    if frame.first().copied() != Some(3) {
        return Ok(None);
    }

    let data_ctx = match connector::legacy::decode_send_data_indication(frame) {
        Ok(data_ctx) => data_ctx,
        Err(err) => {
            trace!(%err, "ignoring non-redirection RDP frame");
            return Ok(None);
        }
    };
    let user_data = data_ctx.user_data;
    if user_data.len() < 6 {
        return Ok(None);
    }

    let total_length = usize::from(u16::from_le_bytes([user_data[0], user_data[1]]));
    let pdu_type_with_version = u16::from_le_bytes([user_data[2], user_data[3]]);
    if pdu_type_with_version & 0x000f != 0x000a {
        return Ok(None);
    }

    let end = total_length.min(user_data.len());
    parse_server_redirection(&user_data[6..end]).map(Some)
}

fn parse_server_redirection(data: &[u8]) -> anyhow::Result<ServerRedirection> {
    const LB_TARGET_NET_ADDRESS: u32 = 0x0000_0001;
    const LB_LOAD_BALANCE_INFO: u32 = 0x0000_0002;
    const LB_USERNAME: u32 = 0x0000_0004;
    const LB_DOMAIN: u32 = 0x0000_0008;
    const LB_PASSWORD: u32 = 0x0000_0010;
    const LB_TARGET_FQDN: u32 = 0x0000_0100;
    const LB_TARGET_NETBIOS_NAME: u32 = 0x0000_0200;
    const LB_CLIENT_TSV_URL: u32 = 0x0000_1000;
    const LB_PASSWORD_IS_PK_ENCRYPTED: u32 = 0x0000_4000;
    const LB_REDIRECTION_GUID: u32 = 0x0000_8000;

    let data = if data.len() >= 4 && u16::from_le_bytes([data[2], data[3]]) == 0x0400 {
        &data[2..]
    } else {
        data
    };

    let mut cursor = ByteCursor::new(data);
    let _flags = cursor.read_u16("server redirection flags")?;
    let length = usize::from(cursor.read_u16("server redirection length")?);
    let packet_len = length.min(data.len());
    let mut cursor = ByteCursor::new(&data[..packet_len]);

    let _flags = cursor.read_u16("server redirection flags")?;
    let _length = cursor.read_u16("server redirection length")?;
    let _session_id = cursor.read_u32("server redirection session ID")?;
    let redir_flags = cursor.read_u32("server redirection flags")?;

    if redir_flags & LB_TARGET_NET_ADDRESS != 0 {
        let _ = cursor.read_unicode_string("redirection target net address")?;
    }

    let routing_token = if redir_flags & LB_LOAD_BALANCE_INFO != 0 {
        let load_balance_info = cursor.read_bytes_with_u32_len("redirection load balance info")?;
        routing_token_from_load_balance_info(load_balance_info)
    } else {
        None
    }
    .with_context(|| {
        format!(
            "RDP server redirection did not include a routing token; redir_flags=0x{redir_flags:08x}; packet={}",
            hex_preview(data, 96)
        )
    })?;

    let username = if redir_flags & LB_USERNAME != 0 {
        Some(cursor.read_unicode_string("redirection username")?)
    } else {
        None
    };
    let domain = if redir_flags & LB_DOMAIN != 0 {
        Some(cursor.read_unicode_string("redirection domain")?)
    } else {
        None
    };
    let password_bytes = if redir_flags & LB_PASSWORD != 0 {
        Some(
            cursor
                .read_bytes_with_u32_len("redirection password")?
                .to_vec(),
        )
    } else {
        None
    };
    let password =
        if redir_flags & LB_PASSWORD != 0 && redir_flags & LB_PASSWORD_IS_PK_ENCRYPTED == 0 {
            password_bytes
                .as_deref()
                .map(|bytes| utf16_string_from_bytes(bytes, "redirection password"))
                .transpose()?
        } else {
            None
        };

    if redir_flags & LB_TARGET_FQDN != 0 {
        let _ = cursor.read_unicode_string("redirection target FQDN")?;
    }
    if redir_flags & LB_TARGET_NETBIOS_NAME != 0 {
        let _ = cursor.read_unicode_string("redirection target NetBIOS name")?;
    }
    if redir_flags & LB_CLIENT_TSV_URL != 0 {
        let _ = cursor.read_bytes_with_u32_len("redirection client TSV URL")?;
    }
    let redirection_guid = if redir_flags & LB_REDIRECTION_GUID != 0 {
        Some(cursor.read_bytes_with_u32_len("redirection GUID")?.to_vec())
    } else {
        None
    };

    let username = username.filter(|value| !value.is_empty());
    let domain = domain.filter(|value| !value.is_empty());
    let password = password.filter(|value| !value.is_empty());
    let redirection_auth = if password_bytes.is_some() || redirection_guid.is_some() {
        Some(RdpRedirectionAuth {
            flags: redir_flags,
            username: username.clone(),
            domain: domain.clone(),
            password: password_bytes,
            redirection_guid,
        })
    } else {
        None
    };

    Ok(ServerRedirection {
        routing_token,
        username,
        password,
        domain,
        redirection_auth,
    })
}

fn routing_token_from_load_balance_info(data: &[u8]) -> Option<String> {
    let value = String::from_utf8_lossy(data);
    let value = value.trim_end_matches(|ch| matches!(ch, '\r' | '\n' | '\0'));
    let token = value
        .strip_prefix("Cookie: msts=")
        .or_else(|| value.strip_prefix("Cookie: MSTS="))
        .or_else(|| value.strip_prefix("msts="))
        .unwrap_or(value)
        .trim()
        .to_owned();

    if token.is_empty() { None } else { Some(token) }
}

fn hex_preview(data: &[u8], max_len: usize) -> String {
    data.iter()
        .take(max_len)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

struct ByteCursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn read_u16(&mut self, field: &str) -> anyhow::Result<u16> {
        let bytes = self.read_exact(2, field)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self, field: &str) -> anyhow::Result<u32> {
        let bytes = self.read_exact(4, field)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_bytes_with_u32_len(&mut self, field: &str) -> anyhow::Result<&'a [u8]> {
        let len = usize::try_from(self.read_u32(field)?).context("redirection length overflow")?;
        self.read_exact(len, field)
    }

    fn read_unicode_string(&mut self, field: &str) -> anyhow::Result<String> {
        let bytes = self.read_bytes_with_u32_len(field)?;
        utf16_string_from_bytes(bytes, field)
    }

    fn read_exact(&mut self, len: usize, field: &str) -> anyhow::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .with_context(|| format!("{field} length overflow"))?;
        let bytes = self
            .data
            .get(self.offset..end)
            .with_context(|| format!("redirection packet is missing {field}"))?;
        self.offset = end;
        Ok(bytes)
    }
}

fn utf16_string_from_bytes(bytes: &[u8], field: &str) -> anyhow::Result<String> {
    if bytes.len() % 2 != 0 {
        bail!("{field} has an odd UTF-16 byte length");
    }

    let mut words = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let word = u16::from_le_bytes([chunk[0], chunk[1]]);
        if word == 0 {
            break;
        }
        words.push(word);
    }
    String::from_utf16(&words).with_context(|| format!("decode {field}"))
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
    fn redirection_detector_ignores_non_tpkt_frames() {
        let result =
            server_redirection_from_frame(&[0, 0, 0, 0]).expect("non-TPKT frame is not fatal");

        assert!(result.is_none());
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
