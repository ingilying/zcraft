use crate::{
    context::{Context, ServerContext},
    handler::{Dispatcher, Session},
    players::Inbox,
    protocol::{Clientbound, State, decode, packets::login::SetCompression},
    transport::Transport,
};
use std::{
    future::{Future, pending, poll_fn},
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
    time::{Instant, MissedTickBehavior, sleep_until, timeout_at},
};

// Preserve the original import path for library users.
pub use crate::config::Config;

pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let server = Arc::new(ServerContext::from_config(&config).await?);
    serve_validated(listener, config, server, shutdown).await
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
    if server.players().max_players() != config.max_players {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "server context player capacity must match config.max_players",
        ));
    }
    if server.play.max_view_distance() != config.max_view_distance {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "server context view distance must match config.max_view_distance",
        ));
    }
    if server.play.game_mode() != config.game_mode {
        return Err(crate::protocol::invalid(
            "server context game mode must match config.game_mode",
        ));
    }
    server.status.validate()?;
    serve_validated(listener, config, server, shutdown).await
}

// Both public entry points validate their inputs before reaching this loop.
async fn serve_validated(
    listener: TcpListener,
    config: Config,
    server: Arc<ServerContext>,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let compression_threshold = usize::try_from(config.compression_threshold).ok();
    let slots = Arc::new(Semaphore::new(config.max_connections));
    let mut tasks = JoinSet::new();
    let mut world_tick = tokio::time::interval_at(
        Instant::now() + crate::world::TICK_INTERVAL,
        crate::world::TICK_INTERVAL,
    );
    world_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    const AUTOSAVE_TICKS: u64 = 6_000; // 5 minutes at 20 TPS.
    let mut ticks_since_save = 0u64;
    let mut autosave: Option<tokio::task::JoinHandle<io::Result<crate::world::SaveSummary>>> = None;
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            res = async { autosave.as_mut().unwrap().await }, if autosave.is_some() => {
                autosave = None;
                match res {
                    Ok(Ok(summary)) => {
                        tracing::info!(chunks = summary.chunks_saved, "autosaved world");
                    }
                    Ok(Err(error)) => {
                        tracing::error!(%error, "world autosave failed");
                    }
                    Err(error) => {
                        tracing::error!(%error, "world autosave task panicked");
                    }
                }
            }
            _ = world_tick.tick() => {
                server.tick();
                ticks_since_save += 1;
                if server.play.world().has_storage() && autosave.is_none() {
                    if ticks_since_save >= AUTOSAVE_TICKS {
                        ticks_since_save = 0;
                        let save_server = server.clone();
                        autosave = Some(tokio::task::spawn_blocking(move || {
                            save_server.play.save_world()
                        }));
                    } else if server.play.world().needs_dirty_flush() {
                        let save_server = server.clone();
                        autosave = Some(tokio::task::spawn_blocking(move || {
                            let chunks_saved = save_server
                                .play
                                .world()
                                .flush_dirty_batch(crate::world::DIRTY_FLUSH_BATCH_SIZE)?;
                            Ok(crate::world::SaveSummary {
                                persisted: true,
                                chunks_saved,
                            })
                        }));
                    }
                }
            }
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
                    if let Err(error) = connection(stream, server, deadline, compression_threshold).await {
                        tracing::debug!(%peer, %error, "connection closed");
                    }
                });
            }
        }
    };
    // Wait for any in-flight autosave, cancel connection tasks (whose drop
    // guards persist online players), then perform a final blocking world save.
    tracing::info!("Stopping the server");
    if let Some(handle) = autosave
        && let Err(error) = handle.await
    {
        tracing::error!(%error, "in-flight autosave task failed during shutdown");
    }
    tracing::info!("Saving players");
    tasks.shutdown().await;
    let save_result = if server.play.world().has_storage() {
        tracing::info!("Saving worlds");
        let save_server = server.clone();
        match tokio::task::spawn_blocking(move || save_server.play.save_world()).await {
            Ok(Ok(summary)) => {
                tracing::info!(chunks = summary.chunks_saved, "Saved world on shutdown");
                Ok(())
            }
            Ok(Err(error)) => {
                tracing::error!(%error, "failed to save world on shutdown");
                Err(error)
            }
            Err(error) => {
                tracing::error!(%error, "shutdown save task panicked");
                Err(io::Error::other(error))
            }
        }
    } else {
        Ok(())
    };
    tracing::info!("Server stopped");
    result.and(save_result)
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

/// Overflow cancels an in-flight socket write as well as idle reads. A partial
/// write cannot safely be retried or followed by a Disconnect; close the socket.
async fn until_lagged<T>(
    inbox: Option<&Inbox>,
    work: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    let cancelled = async {
        match inbox {
            Some(inbox) => inbox.cancelled().await,
            None => pending::<()>().await,
        }
    };
    tokio::select! {
        biased;
        _ = cancelled => Err(io::Error::other("player broadcast queue overflow")),
        result = work => result,
    }
}

async fn write_until<S: AsyncRead + AsyncWrite + Unpin>(
    transport: &mut Transport<S>,
    body: &[u8],
    until: Instant,
    inbox: Option<&Inbox>,
) -> io::Result<()> {
    until_lagged(inbox, async {
        timeout_at(until, transport.write_frame(body))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "response write timed out"))?
    })
    .await
}

async fn connection(
    stream: TcpStream,
    server: Arc<ServerContext>,
    deadline: Duration,
    compression_threshold: Option<usize>,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    run_connection(
        Transport::new(stream),
        &server,
        deadline,
        compression_threshold,
    )
    .await
}

enum ConnectionEvent {
    Frame(Vec<u8>),
    Broadcast(crate::players::Delivery),
    Tick,
    Chunks,
    Eof,
}

struct ConnectionSession<'a> {
    session: Session,
    server: &'a ServerContext,
}

impl std::ops::Deref for ConnectionSession<'_> {
    type Target = Session;
    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl std::ops::DerefMut for ConnectionSession<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session
    }
}

impl Drop for ConnectionSession<'_> {
    fn drop(&mut self) {
        crate::handler::play::save_session(
            &mut self.session,
            &self.server.play,
            self.server.players(),
        );
    }
}

async fn run_connection<S: AsyncRead + AsyncWrite + Unpin>(
    mut transport: Transport<S>,
    server: &ServerContext,
    deadline: Duration,
    compression_threshold: Option<usize>,
) -> io::Result<()> {
    let mut session = ConnectionSession {
        session: Session::default(),
        server,
    };
    let login_deadline = Instant::now() + Duration::from_secs(30);
    let mut frame_deadline = Instant::now() + deadline;
    let mut ticks = tokio::time::interval(Duration::from_millis(200));
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let inbox = session
            .reservation
            .as_ref()
            .map(|reservation| reservation.inbox());
        let state_before = session.state;
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
        let chunk_update = if state_before == State::Play && configuration_deadline.is_none() {
            crate::handler::play::next_chunk_update(&session)
        } else {
            None
        };
        let event = until_lagged(inbox.as_ref(), async {
            tokio::select! {
                frame = read_frame_until(&mut transport, reading_until, deadline) => {
                    Ok(frame?.map_or(ConnectionEvent::Eof, ConnectionEvent::Frame))
                }
                _ = ticks.tick(), if state_before == State::Play || configuration_deadline.is_some() => {
                    Ok(ConnectionEvent::Tick)
                }
                _ = async {
                    match chunk_update {
                        Some(at) => sleep_until(at).await,
                        None => pending().await,
                    }
                } => Ok(ConnectionEvent::Chunks),
                body = async {
                    match &inbox {
                        Some(inbox) => inbox.next().await,
                        None => pending().await,
                    }
                }, if state_before == State::Play && configuration_deadline.is_none() => {
                    Ok(ConnectionEvent::Broadcast(body))
                }
            }
        })
        .await;
        let received_frame = matches!(event, Ok(ConnectionEvent::Frame(_)));
        let result = match event {
            Ok(ConnectionEvent::Frame(frame)) => {
                until_lagged(inbox.as_ref(), async {
                    let packet = decode(session.state, &frame)?;
                    let handling_until = configuration_deadline.unwrap_or_else(|| {
                        if state_before == State::Play {
                            Instant::now() + deadline
                        } else {
                            login_deadline
                        }
                    });
                    timeout_at(
                        handling_until,
                        Dispatcher.dispatch(
                            &mut Context {
                                session: &mut session,
                                server,
                            },
                            packet,
                        ),
                    )
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "packet handling timed out")
                    })?
                })
                .await
            }
            Ok(ConnectionEvent::Tick) if configuration_deadline.is_some() => {
                crate::handler::configuration::tick(&mut session, Instant::now())
            }
            Ok(ConnectionEvent::Tick) => crate::handler::play::tick(&mut session, Instant::now()),
            Ok(ConnectionEvent::Chunks) => {
                crate::handler::play::stream_chunks(&mut session, &server.play, Instant::now())
            }
            Ok(ConnectionEvent::Broadcast(crate::players::Delivery::Control(control))) => {
                crate::handler::play::apply_control(
                    &mut session,
                    &server.play,
                    server.players(),
                    control,
                )
            }
            Ok(ConnectionEvent::Broadcast(delivery)) => {
                let writing_until = Instant::now() + deadline;
                for body in delivery.bodies()? {
                    write_until(&mut transport, &body, writing_until, inbox.as_ref()).await?;
                }
                continue;
            }
            Ok(ConnectionEvent::Eof) => return Ok(()),
            Err(error) => Err(error),
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
                        let _ = write_until(
                            &mut transport,
                            &body,
                            Instant::now() + deadline,
                            inbox.as_ref(),
                        )
                        .await;
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
            if let (Clientbound::LoginSuccess(_), Some(threshold)) = (&reply, compression_threshold)
            {
                // Encryption, if negotiated, is already active. Only the
                // framing changes after this packet is completely sent.
                let packet = Clientbound::from(SetCompression {
                    threshold: threshold as i32,
                });
                write_until(
                    &mut transport,
                    &packet.encode()?,
                    writing_until,
                    inbox.as_ref(),
                )
                .await?;
                transport.enable_compression(threshold)?;
            }
            let body = reply.encode()?;
            write_until(&mut transport, &body, writing_until, inbox.as_ref()).await?;
            let sent = Instant::now();
            crate::handler::play::packet_sent(&mut session, &reply, sent)?;
            crate::handler::configuration::packet_sent(&mut session, &reply, sent);
        }
        if received_frame {
            frame_deadline = Instant::now() + deadline;
        }
        if outcome.close {
            return Ok(());
        }
        if state_before == State::Configuration && session.state == State::Play {
            session.play_initialized();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        players::{BROADCAST_QUEUE_CAPACITY, Players},
        protocol::packets::play::TimeUpdate,
    };
    use tokio::io::{AsyncReadExt, duplex};
    use uuid::Uuid;

    #[tokio::test]
    async fn overflow_cancels_a_partial_write_without_waiting_for_io_timeout() {
        let players = Players::new(1);
        let (socket, mut peer) = duplex(1);
        let slot = players.reserve(Uuid::nil()).unwrap();
        slot.enter_play();
        let task = tokio::spawn(async move {
            let mut transport = Transport::new(socket);
            let inbox = slot.inbox();
            write_until(
                &mut transport,
                &[0; 20],
                Instant::now() + Duration::from_secs(60),
                Some(&inbox),
            )
            .await
        });
        // Wait until the write starts, then let it stall in the one-byte buffer.
        assert_eq!(peer.read_u8().await.unwrap(), 20);
        let packet = TimeUpdate::default().into();
        for _ in 0..BROADCAST_QUEUE_CAPACITY {
            assert_eq!(players.broadcast(&packet).unwrap().queued, 1);
        }
        assert_eq!(players.broadcast(&packet).unwrap().lagged, 1);
        let error = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "player broadcast queue overflow");
        assert_eq!(players.online_count(), 0);
        assert!(players.reserve(Uuid::nil()).is_ok());
    }

    #[tokio::test]
    async fn aborting_a_task_releases_its_ownership_even_with_an_inbox_clone() {
        let players = Players::new(1);
        let slot = players.reserve(Uuid::nil()).unwrap();
        slot.enter_play();
        let _inbox = slot.inbox();
        let task = tokio::spawn(async move {
            let _owner = slot;
            pending::<()>().await;
        });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(players.online_count(), 0);
        assert!(players.reserve(Uuid::nil()).is_ok());
    }
}
