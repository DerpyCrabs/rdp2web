use rdp2web::rdp::{ServerEvent, ServerStatus};
use serial_test::serial;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

fn load_env() {
    rdp2web::config::load_dotenv();
}

#[test]
fn selected_env_contains_required_rdp_settings() {
    load_env();

    for key in [
        "RDP_HOST",
        "RDP_PORT",
        "RDP_USER",
        "RDP_PASSWORD",
        "UI_PORT",
    ] {
        let value = std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set"));
        assert!(!value.trim().is_empty(), "{key} must not be empty");
    }
}

#[test]
fn configured_rdp_port_accepts_tcp() {
    load_env();

    let host = std::env::var("RDP_HOST").expect("RDP_HOST must be set");
    let port = std::env::var("RDP_PORT")
        .expect("RDP_PORT must be set")
        .parse::<u16>()
        .expect("RDP_PORT must be a u16");
    let addr = format!("{host}:{port}");
    let socket = addr
        .to_socket_addrs()
        .expect("RDP address must resolve")
        .next()
        .expect("RDP address must resolve to at least one socket");

    TcpStream::connect_timeout(&socket, Duration::from_secs(3))
        .unwrap_or_else(|err| panic!("RDP endpoint {addr} must accept TCP connections: {err}"));
}

#[test]
#[serial(rdp)]
fn configured_rdp_login_succeeds() {
    load_env();

    let app_config = rdp2web::config::AppConfig::from_env().expect("app config must load");
    let (width, height) =
        rdp2web::rdp::validate_rdp_login(app_config.rdp).expect("RDP login must succeed");

    assert!(width >= 200, "RDP desktop width should be usable");
    assert!(height >= 200, "RDP desktop height should be usable");
}

#[tokio::test]
#[serial(rdp)]
async fn configured_rdp_streams_direct_h264_video() {
    load_env();

    let app_config = rdp2web::config::AppConfig::from_env().expect("app config must load");
    let (control_tx, mut server_rx) = rdp2web::rdp::start_rdp_session(app_config.rdp);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut connected = false;
    let mut desktop_size = None;
    let mut saw_key = false;
    let mut video_frames = 0usize;

    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };

        let event = match tokio::time::timeout(remaining, server_rx.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => panic!("RDP session should remain open until a video frame is received"),
            Err(_) => panic!("RDP session never produced a direct H.264 frame"),
        };

        match event {
            ServerEvent::Status(ServerStatus::Connected) => connected = true,
            ServerEvent::Status(ServerStatus::DesktopSize { width, height }) => {
                assert!(width >= 200, "RDP desktop width should be usable");
                assert!(height >= 200, "RDP desktop height should be usable");
                desktop_size = Some((width, height));
            }
            ServerEvent::Status(ServerStatus::Error { message }) => {
                panic!("RDP session failed before producing video: {message}");
            }
            ServerEvent::Status(ServerStatus::Disconnected { reason }) => {
                panic!("RDP session disconnected before producing video: {reason}");
            }
            ServerEvent::Status(ServerStatus::Connecting) => {}
            ServerEvent::Video(frame) => {
                assert!(connected, "video should arrive after connected status");
                assert!(
                    desktop_size.is_some(),
                    "desktop size should be reported before video"
                );
                assert!(
                    frame.data.windows(4).any(|window| window == [0, 0, 0, 1]),
                    "direct video payload must be Annex B H.264"
                );
                assert_ne!(
                    frame.data.get(..4),
                    Some(b"R2W3".as_slice()),
                    "direct video must not use the raw RGBA patch protocol"
                );
                assert!(
                    frame.data.len() < 2 * 1024 * 1024,
                    "single H.264 frame is implausibly large: {} bytes",
                    frame.data.len()
                );
                saw_key |= frame.key;
                video_frames += 1;
                if saw_key && video_frames >= 3 {
                    drop(control_tx);
                    return;
                }
            }
        }
    }

    drop(control_tx);
    panic!("received no key H.264 frame before timeout");
}

#[tokio::test]
#[serial(rdp)]
async fn configured_rdp_direct_h264_stream_remains_live_locally() {
    load_env();

    let app_config = rdp2web::config::AppConfig::from_env().expect("app config must load");
    let (control_tx, mut server_rx) = rdp2web::rdp::start_rdp_session(app_config.rdp);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut sample_started = None;
    let mut frames = 0usize;
    let mut bytes = 0usize;

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let event = tokio::time::timeout(remaining, server_rx.recv())
            .await
            .expect("RDP session should produce direct video before timeout")
            .expect("RDP session should remain open during fps sample");

        match event {
            ServerEvent::Status(ServerStatus::Error { message }) => {
                panic!("RDP session failed during fps sample: {message}");
            }
            ServerEvent::Status(ServerStatus::Disconnected { reason }) => {
                panic!("RDP session disconnected during fps sample: {reason}");
            }
            ServerEvent::Status(
                ServerStatus::Connecting
                | ServerStatus::Connected
                | ServerStatus::DesktopSize { .. },
            ) => {}
            ServerEvent::Video(frame) => {
                let started = *sample_started.get_or_insert_with(Instant::now);
                frames += 1;
                bytes += frame.data.len();
                if started.elapsed() >= Duration::from_secs(3) {
                    let fps = frames as f64 / started.elapsed().as_secs_f64();
                    let mbps = bytes as f64 * 8.0 / started.elapsed().as_secs_f64() / 1_000_000.0;
                    drop(control_tx);
                    assert!(
                        frames >= 3,
                        "local direct H.264 stream stalled: {frames} frames in {:.1}s",
                        started.elapsed().as_secs_f64()
                    );
                    assert!(bytes > 0, "local direct H.264 stream produced empty frames");
                    eprintln!("idle damaged-frame cadence: {fps:.1} fps, {mbps:.1} Mbps");
                    return;
                }
            }
        }
    }

    drop(control_tx);
    assert!(
        frames > 0,
        "RDP session never produced direct video frames for fps sample"
    );
}
