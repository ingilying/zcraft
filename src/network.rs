use crate::{
    context::{Context, ServerContext},
    handler::{Dispatcher, Session, login::LoginService, status::StatusService},
    protocol::decode,
    transport::Transport,
};
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};

// Preserve the original import path for library users.
pub use crate::config::Config;

pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    config.validate()?;
    let server = Arc::new(ServerContext {
        status: StatusService::from_config(&config)?,
        login: LoginService::from_config(&config).await?,
    });
    serve_with_context(listener, config, server, shutdown).await
}

/// Entry point for embedders/tests supplying services. Application startup must
/// perform its EULA check before creating the listener.
pub async fn serve_with_context(
    listener: TcpListener,
    config: Config,
    server: Arc<ServerContext>,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    config.validate()?;
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
                let server = server.clone();
                let deadline = config.io_timeout;
                tasks.spawn(async move {
                    let _permit = permit;
                    match timeout(Duration::from_secs(30), connection(stream, server, deadline)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::debug!(%peer, %error, "connection closed"),
                        Err(_) => tracing::debug!(%peer, "login/status session timed out"),
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
    stream: TcpStream,
    server: Arc<ServerContext>,
    deadline: Duration,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut transport = Transport::new(stream);
    let mut session = Session::default();
    loop {
        let frame = timeout(deadline, transport.read_frame())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "frame read timed out"))??;
        let Some(frame) = frame else {
            return Ok(());
        };
        let packet = decode(session.state, &frame)?;
        let outcome = Dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server,
                },
                packet,
            )
            .await?;
        if let Some(secret) = outcome.encryption {
            transport.enable_encryption(&secret)?;
        }
        if let Some(reply) = outcome.reply {
            let body = reply.encode()?;
            timeout(deadline, transport.write_frame(&body))
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
