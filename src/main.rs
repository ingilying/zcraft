use std::{io, process::ExitCode};
use tokio::net::TcpListener;
use zcraft::{
    config::{Config, DEFAULT_CONFIG_PATH},
    eula::{DEFAULT_EULA_PATH, require_acceptance},
    network::serve,
};

fn env_override(name: &str) -> io::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name}: {error}"),
        )),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> io::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let path = std::env::var_os("ZCRAFT_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.into());
    let mut config = Config::load_or_create(&path)?;
    config.apply_overrides(env_override("ZCRAFT_BIND")?, env_override("ZCRAFT_MOTD")?)?;
    tracing::info!(path = %path.display(), "configuration loaded");
    require_acceptance(DEFAULT_EULA_PATH)?;
    if config.offline_mode {
        tracing::warn!(
            "Offline mode enabled: usernames are NOT authenticated. Do not expose this server to untrusted networks."
        );
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
