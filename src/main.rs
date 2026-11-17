use std::io;
use tokio::net::TcpListener;
use zcraft::network::{Config, serve};

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut config = Config::default();
    if let Ok(bind) = std::env::var("ZCRAFT_BIND") {
        config.bind = bind;
    }
    if let Ok(motd) = std::env::var("ZCRAFT_MOTD") {
        config.motd = motd;
    }
    let listener = TcpListener::bind(&config.bind).await?;
    tracing::info!(address = %listener.local_addr()?, "ZCraft listening (Java 1.21.1 / 767)");
    serve(listener, config, async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to listen for Ctrl-C");
        }
    })
    .await
}
