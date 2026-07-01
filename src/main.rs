use anyhow::Context;
use rdp2web::{config, web};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    config::load_dotenv();
    init_tracing();

    let config = config::AppConfig::from_env().context("load configuration")?;
    web::serve(config).await
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::prelude::*;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("rdp2web=info,tower_http=info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().compact())
        .try_init();
}
