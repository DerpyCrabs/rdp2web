use anyhow::{Context, bail};
use serde::Serialize;
use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

const FALLBACK_DESKTOP_SIZE: (u16, u16) = (1280, 720);
const DEFAULT_TLS_CERT_PATH: &str = "certs/rdp2web.crt";
const DEFAULT_TLS_KEY_PATH: &str = "certs/rdp2web.key";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub ui_bind: SocketAddr,
    pub tls: TlsConfig,
    pub rdp: RdpConfig,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RdpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Serialize)]
pub struct PublicConfig {
    pub default_width: u16,
    pub default_height: u16,
}

impl AppConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let width = optional_u16_env("RDP_WIDTH")?.unwrap_or(FALLBACK_DESKTOP_SIZE.0);
        let height = optional_u16_env("RDP_HEIGHT")?.unwrap_or(FALLBACK_DESKTOP_SIZE.1);

        let rdp = RdpConfig {
            host: env_var("RDP_HOST")?,
            port: env_var("RDP_PORT")?
                .parse()
                .context("RDP_PORT must be a u16")?,
            username: env_var("RDP_USER")?,
            password: env_var("RDP_PASSWORD")?,
            domain: env::var("RDP_DOMAIN")
                .ok()
                .filter(|value| !value.is_empty()),
            width,
            height,
        };
        if rdp.width < 200 || rdp.height < 200 {
            bail!("RDP_WIDTH and RDP_HEIGHT must be at least 200");
        }

        let ui_port = env::var("UI_PORT")
            .unwrap_or_else(|_| "8081".to_owned())
            .parse()
            .context("UI_PORT must be a u16")?;
        let ui_host = env::var("UI_HOST").unwrap_or_else(|_| "0.0.0.0".to_owned());
        let ui_ip: IpAddr = ui_host.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let tls = tls_config_from_env()?;

        Ok(Self {
            ui_bind: SocketAddr::new(ui_ip, ui_port),
            tls,
            rdp,
        })
    }

    pub fn public_config(&self) -> PublicConfig {
        PublicConfig {
            default_width: self.rdp.width,
            default_height: self.rdp.height,
        }
    }
}

fn tls_config_from_env() -> anyhow::Result<TlsConfig> {
    let cert_path = env::var("UI_TLS_CERT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);
    let key_path = env::var("UI_TLS_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);

    match (cert_path, key_path) {
        (Some(cert_path), Some(key_path)) => Ok(TlsConfig {
            cert_path,
            key_path,
        }),
        (Some(_), None) => bail!("UI_TLS_KEY is required when UI_TLS_CERT is set"),
        (None, Some(_)) => bail!("UI_TLS_CERT is required when UI_TLS_KEY is set"),
        (None, None)
            if Path::new(DEFAULT_TLS_CERT_PATH).exists()
                && Path::new(DEFAULT_TLS_KEY_PATH).exists() =>
        {
            Ok(TlsConfig {
                cert_path: PathBuf::from(DEFAULT_TLS_CERT_PATH),
                key_path: PathBuf::from(DEFAULT_TLS_KEY_PATH),
            })
        }
        (None, None) => bail!(
            "UI TLS certificate is required; set UI_TLS_CERT/UI_TLS_KEY or create {DEFAULT_TLS_CERT_PATH} and {DEFAULT_TLS_KEY_PATH}"
        ),
    }
}

fn env_var(name: &str) -> anyhow::Result<String> {
    let value = env::var(name).with_context(|| format!("{name} is required"))?;
    if value.trim().is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(value)
}

fn optional_u16_env(name: &str) -> anyhow::Result<Option<u16>> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => value
            .parse()
            .with_context(|| format!("{name} must be a u16"))
            .map(Some),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_config_excludes_credentials() {
        let config = AppConfig {
            ui_bind: "0.0.0.0:8081".parse().unwrap(),
            tls: TlsConfig {
                cert_path: PathBuf::from("test.crt"),
                key_path: PathBuf::from("test.key"),
            },
            rdp: RdpConfig {
                host: "127.0.0.1".to_owned(),
                port: 3389,
                username: "user".to_owned(),
                password: "secret".to_owned(),
                domain: None,
                width: 1280,
                height: 720,
            },
        };

        let json = serde_json::to_string(&config.public_config()).unwrap();
        assert!(!json.contains("127.0.0.1"));
        assert!(!json.contains("user"));
        assert!(!json.contains("secret"));
    }
}
