use crate::{
    context::{Context, ServerContext},
    handler::{Dispatcher, Session, login::LoginService, status::StatusService},
    protocol::{State, decode},
    transport::Transport,
};
use std::{
    future::{Future, poll_fn},
    io,
    pin::pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
    time::{Instant, MissedTickBehavior, sleep_until, timeout, timeout_at},
};

// Preserve the original import path for library users.
pub use crate::config::Config;

pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    config.validate()?;
    let mut configuration = crate::handler::configuration::ConfigurationService::new()?;
    if let Some(pack) = &config.resource_pack {
        configuration = configuration.with_resource_pack(pack.clone())?;
    }
    let server = Arc::new(ServerContext {
        status: StatusService::from_config(&config)?,
        login: LoginService::from_config(&config).await?,
        configuration,
        play: crate::handler::play::PlayService::new()?,
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

/// Transport reads are cancellation-safe. Poll them alongside a timer whose
/// deadline tightens as soon as the first byte arrives, rather than waiting for
/// the next heartbeat tick to notice a partial frame.
async fn read_frame_until<S: AsyncRead + AsyncWrite + Unpin>(
    transport: &mut Transport<S>,
    until: Instant,
    frame_timeout: Duration,
) -> io::Result<Option<Vec<u8>>> {
    let mut timer = pin!(sleep_until(until));
    poll_fn(|cx| {
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "frame or protocol phase deadline exceeded",
            )));
        }
        let result = {
            let read = transport.read_frame();
            pin!(read).poll(cx)
        };
        if result.is_pending()
            && let Some(start) = transport.partial_frame_started()
        {
            timer.as_mut().reset(until.min(start + frame_timeout));
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "partial frame deadline exceeded",
                )));
            }
        }
        result
    })
    .await
}

async fn connection(
    stream: TcpStream,
    server: Arc<ServerContext>,
    deadline: Duration,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut transport = Transport::new(stream);
    let mut session = Session::default();
    let login_deadline = Instant::now() + Duration::from_secs(30);
    let mut frame_deadline = Instant::now() + deadline;
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let configuration_deadline = crate::handler::configuration::deadline(&session);
        let reading_until = if let Some(phase_deadline) = configuration_deadline {
            // Idle Configuration waits for protocol progress/heartbeat. A partial
            // frame has its own absolute I/O bound; neither ticks nor other
            // packets can extend the absolute phase deadline.
            transport
                .partial_frame_started()
                .map(|start| (start + deadline).min(phase_deadline))
                .unwrap_or(phase_deadline)
        } else if session.state == State::Play {
            // Idle Play sockets are governed by Keep Alive. Partially received
            // frames still have an absolute deadline that ticks cannot reset.
            transport
                .partial_frame_started()
                .map(|start| start + deadline)
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600))
        } else {
            frame_deadline.min(login_deadline)
        };
        let mut received_frame = false;
        let result: io::Result<crate::handler::Outcome> = tokio::select! {
            result = async {
                let frame = read_frame_until(&mut transport, reading_until, deadline).await?;
                let Some(frame) = frame else { return Ok(None); };
                received_frame = true;
                let packet = decode(session.state, &frame)?;
                let handling_until = configuration_deadline.unwrap_or_else(|| {
                    if session.state == State::Play { Instant::now() + deadline } else { login_deadline }
                });
                let outcome = timeout_at(handling_until, Dispatcher.dispatch(
                    &mut Context { session: &mut session, server: &server }, packet
                )).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "packet handling timed out"))??;
                Ok(Some(outcome))
            } => match result {
                Ok(Some(outcome)) => Ok(outcome),
                Ok(None) => return Ok(()),
                Err(error) => Err(error),
            },
            _ = ticks.tick(), if session.state == State::Play || configuration_deadline.is_some() => {
                if configuration_deadline.is_some() {
                    crate::handler::configuration::tick(&mut session, Instant::now())
                } else {
                    crate::handler::play::tick(&mut session, Instant::now())
                }
            }
        };
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                if session.state == State::Configuration || configuration_deadline.is_some() {
                    let reason = if error.kind() == io::ErrorKind::TimedOut {
                        "ZCraft: configuration timed out."
                    } else {
                        "ZCraft: invalid configuration packet."
                    };
                    // Best effort only: a broken/non-reading peer must not keep
                    // the connection task alive while we report its error.
                    let outcome = crate::handler::configuration::disconnect(&session, reason);
                    if let Ok(body) = outcome.replies[0].encode() {
                        let _ = timeout(deadline, transport.write_frame(&body)).await;
                    }
                }
                return Err(error);
            }
        };
        if let Some(secret) = outcome.encryption {
            transport.enable_encryption(&secret)?;
        }
        // One bound for the whole batch, not a fresh allowance for every
        // registry/chunk. Response windows are armed only after writes finish.
        let writing_until = Instant::now() + deadline;
        for reply in outcome.replies {
            let body = reply.encode()?;
            timeout_at(writing_until, transport.write_frame(&body))
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "response write timed out")
                })??;
            let sent = Instant::now();
            crate::handler::play::packet_sent(&mut session, &reply, sent);
            crate::handler::configuration::packet_sent(&mut session, &reply, sent);
        }
        if received_frame {
            frame_deadline = Instant::now() + deadline;
        }
        if outcome.close {
            return Ok(());
        }
    }
}
