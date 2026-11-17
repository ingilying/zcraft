use crate::{
    context::{Context, ServerContext},
    handler::{Dispatcher, Session, status::StatusService},
    protocol::{decode, read_frame, write_frame},
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
    });
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
                    if let Err(error) = connection(stream, server, deadline).await {
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
    server: Arc<ServerContext>,
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
        let outcome = Dispatcher.dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            packet,
        )?;
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
