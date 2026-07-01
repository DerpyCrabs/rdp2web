# rdp2web

Rust/Axum server that exposes a local web page for controlling this machine over
RDP. The browser talks to the server over WebSocket; the server keeps the RDP
credentials private and connects to GNOME Remote Desktop with IronRDP.

## Configuration

Create `.env` from `.env.example`:

```dotenv
RDP_HOST=127.0.0.1
RDP_PORT=3389
RDP_USER=your-user
RDP_PASSWORD=your-password
UI_PORT=8081
```

Optional settings:

- `UI_HOST`: bind address for the web server. Defaults to `0.0.0.0`.
- `UI_TLS_CERT` and `UI_TLS_KEY`: PEM certificate and private key for the web
  server. If unset, `certs/rdp2web.crt` and `certs/rdp2web.key` are required.
- `RDP_DOMAIN`: optional RDP domain.
- `RDP_MODE`: RDP target type. Use `shared` for GNOME Screen Sharing / Remote
  Assistance endpoints and `session` for GNOME Remote Login / headless session
  endpoints. Defaults to `shared`.
- `RDP_WIDTH` and `RDP_HEIGHT`: initial RDP desktop size request. Defaults
  to `1280x720`.
- `RDP_ENV_FILE`: optional path to an alternate local env file. If unset, the
  app loads `.env`.

The browser UI is HTTPS-only because LAN WebCodecs requires a secure context.
Create a local certificate that includes the LAN IP in `subjectAltName`, or set
`UI_TLS_CERT` and `UI_TLS_KEY` to an existing trusted certificate.

The web page can request Auto sizing based on the browser tab, or a fixed common
resolution from the header control. Those values are sent as RDP size requests
without editing the env file.

The web UI has no separate authentication layer. Bind it to a trusted interface
or protect it with a firewall/reverse proxy before exposing it beyond the local
network.

## Run

```sh
cargo run
```

Open the logged URL, or use `https://<this-pc-lan-ip>:<UI_PORT>/` from another
device on the local network.

## Test

```sh
cargo test
```

The integration tests load the same env settings as the app and validate this PC
by checking:

- required RDP settings are present,
- the configured RDP port accepts TCP,
- the configured GNOME RDP login completes successfully,
- the RDP graphics path produces direct AVC/H.264 video frames.

## IronRDP Patch

GNOME Remote Desktop requires the RDP Graphics Pipeline and direct AVC/H.264.
This repository patches `ironrdp-connector` and `ironrdp-egfx` through
`[patch.crates-io]` in `Cargo.toml` so the connector advertises graphics
pipeline support and the EGFX client forwards encoded AVC420 frames to the web
client instead of requiring an in-process Rust H.264 decoder.
