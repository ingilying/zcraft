use crate::{
    handler::{Dispatcher, Session},
    protocol::{decode, read_frame, write_frame},
};
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub motd: String,
    pub max_players: u32,
    pub max_connections: usize,
    /// Deadline for a complete frame or response, not per-byte activity.
    pub io_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:25565".into(),
            motd: "ZCraft — Rust Minecraft Server".into(),
            max_players: 20,
            max_connections: 1024,
            io_timeout: Duration::from_secs(10),
        }
    }
}

pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    if config.max_connections == 0 || config.io_timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "connection limit and timeout must be positive",
        ));
    }
    let dispatcher = Arc::new(Dispatcher::new(&config.motd, config.max_players));
    let slots = Arc::new(Semaphore::new(config.max_connections));
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = result { tracing::warn!(%error, "connection task failed"); }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => break Err(error),
                };
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let dispatcher = dispatcher.clone();
                let deadline = config.io_timeout;
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = connection(stream, dispatcher, deadline).await {
                        tracing::debug!(%peer, %error, "connection closed");
                    }
                });
            }
        }
    };
    // Cancel idle reads and release sockets/permits before returning.
    tasks.shutdown().await;
    result
}

async fn connection(
    mut stream: TcpStream,
    dispatcher: Arc<Dispatcher>,
    deadline: Duration,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut session = Session::default();
    loop {
        let frame = timeout(deadline, read_frame(&mut stream))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "frame read timed out"))??;
        let Some(frame) = frame else {
            return Ok(());
        };
        let packet = decode(session.state, &frame)?;
        let outcome = dispatcher.dispatch(&mut session, packet)?;
        if let Some(reply) = outcome.reply {
            let body = reply.encode()?;
            timeout(deadline, write_frame(&mut stream, &body))
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "response write timed out")
                })??;
        }
        if outcome.close {
            return Ok(());
        }
    }
}
