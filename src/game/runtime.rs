//! Dedicated 20 TPS fixed-timestep OS game loop and asynchronous outbound sync
//! worker, completely decoupled from network sockets and protocol framing.
use super::{DropSnapshot, GameEngine, GameInput, GameOutput, Inventory, PlayerSnapshot};
use crate::world;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use uuid::Uuid;

/// Bounded capacity of the inbound ECS game-loop command queue.
/// Prevents unbounded memory growth under packet bursts or lagging simulation.
pub const GAME_INPUT_QUEUE_CAPACITY: usize = 4096;

/// Maximum number of queued commands drained before re-checking the 20 TPS tick deadline.
/// Prevents sustained input floods from starving periodic world/entity simulation ticks.
pub const MAX_INPUT_BATCH_PER_TURN: usize = 256;

/// Minimum interval between slow-tick overload warnings (`Can't keep up!`).
const OVERLOAD_WARN_INTERVAL: Duration = Duration::from_secs(5);

pub enum Reply<T> {
    Sync(SyncSender<T>),
    Async(tokio::sync::oneshot::Sender<T>),
}

impl<T> Reply<T> {
    pub fn send(self, value: T) {
        match self {
            Self::Sync(tx) => {
                let _ = tx.send(value);
            }
            Self::Async(tx) => {
                let _ = tx.send(value);
            }
        }
    }
}

pub enum EngineCommand {
    /// Fire-and-forget gameplay input (movement, digging, placing, inventory, etc.).
    Input(GameInput),
    /// Synchronous/async FIFO barrier input (e.g. PlayerJoin, PlayerLeave, TeleportConfirm)
    /// executed strictly on the next 20 TPS tick boundary after all preceding queued inputs.
    InputSync {
        input: GameInput,
        done: Reply<io::Result<()>>,
    },
    /// Authoritative 20 TPS tick (for explicit external `ServerContext::tick()` calls).
    Tick {
        done: Reply<io::Result<()>>,
    },
    /// Lock-free read queries routed to the exclusive ECS owner thread.
    PlayerSnapshot {
        uuid: Uuid,
        reply: Reply<Option<PlayerSnapshot>>,
    },
    PlayerInventory {
        uuid: Uuid,
        reply: Reply<Option<Inventory>>,
    },
    ExistingDrops {
        reply: Reply<Vec<DropSnapshot>>,
    },
    /// Graceful shutdown signal: drain remaining queued commands and return `GameEngine`.
    Shutdown,
}

struct OutputBatch {
    outputs: Vec<GameOutput>,
    barriers: Vec<Reply<io::Result<()>>>,
}

/// Handle owning the dedicated 20 TPS game-loop thread and its outbound sync worker.
/// Dropping or calling `shutdown()` stops the loop cleanly, drains remaining queued
/// commands, flushes all outbound sync batches, and restores `GameEngine` ownership.
pub struct GameRuntime {
    loop_tx: Arc<Mutex<Option<SyncSender<EngineCommand>>>>,
    tx: Option<SyncSender<EngineCommand>>,
    thread: Option<JoinHandle<()>>,
    sync_thread: Option<JoinHandle<()>>,
}

impl GameRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn start<F>(
        engine: GameEngine,
        engine_slot: Arc<Mutex<Option<GameEngine>>>,
        loop_tx_slot: Arc<Mutex<Option<SyncSender<EngineCommand>>>>,
        capacity: usize,
        mspt_micros: Arc<AtomicU64>,
        tick_notify: Option<tokio::sync::mpsc::Sender<()>>,
        on_outputs: F,
    ) -> io::Result<Self>
    where
        F: FnMut(Vec<GameOutput>) -> io::Result<()> + Send + 'static,
    {
        let (output_tx, output_rx) = mpsc::channel::<OutputBatch>();
        let sync_thread = match thread::Builder::new()
            .name("zcraft-output-sync".into())
            .spawn(move || {
                run_output_sync_thread(output_rx, on_outputs);
            }) {
            Ok(handle) => handle,
            Err(error) => {
                *engine_slot.lock().expect("game engine poisoned") = Some(engine);
                return Err(error);
            }
        };

        let (tx, rx) = mpsc::sync_channel(capacity.max(1));
        *loop_tx_slot.lock().expect("loop_tx poisoned") = Some(tx.clone());

        let loop_engine_slot = engine_slot.clone();
        let loop_tx_inner = loop_tx_slot.clone();
        let thread = match thread::Builder::new()
            .name("zcraft-game-loop".into())
            .spawn(move || {
                run_game_loop_thread(
                    engine,
                    loop_engine_slot,
                    loop_tx_inner,
                    rx,
                    output_tx,
                    mspt_micros,
                    tick_notify,
                );
            }) {
            Ok(handle) => handle,
            Err(error) => {
                *loop_tx_slot.lock().expect("loop_tx poisoned") = None;
                let _ = sync_thread.join();
                return Err(error);
            }
        };

        Ok(Self {
            loop_tx: loop_tx_slot,
            tx: Some(tx),
            thread: Some(thread),
            sync_thread: Some(sync_thread),
        })
    }

    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        // 1. Detach the shared sender slot first so no new commands can be enqueued.
        *self.loop_tx.lock().expect("loop_tx poisoned") = None;
        if let Some(tx) = self.tx.take() {
            // Best-effort wake if idle; dropping `tx` also disconnects `rx` even if full.
            let _ = tx.try_send(EngineCommand::Shutdown);
        }
        if let Some(handle) = self.thread.take()
            && let Err(error) = handle.join()
        {
            tracing::error!(?error, "dedicated game loop thread panicked");
        }
        // 2. Game loop has drained all remaining inputs and dropped `output_tx`.
        //    Wait for the outbound sync worker to finish applying all batches and drop `on_outputs`.
        if let Some(handle) = self.sync_thread.take()
            && let Err(error) = handle.join()
        {
            tracing::error!(?error, "outbound sync worker thread panicked");
        }
    }
}

impl Drop for GameRuntime {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Non-blocking send onto the bounded `EngineCommand` channel for Tokio callers:
/// uses fast-path `try_send` and only offloads to `spawn_blocking` when full.
pub async fn send_command_async(
    tx: SyncSender<EngineCommand>,
    cmd: EngineCommand,
) -> Result<(), EngineCommand> {
    match tx.try_send(cmd) {
        Ok(()) => Ok(()),
        Err(TrySendError::Disconnected(unsent)) => Err(unsent),
        Err(TrySendError::Full(cmd)) => tokio::task::spawn_blocking(move || tx.send(cmd))
            .await
            .map_err(|_| EngineCommand::Shutdown)?
            .map_err(|mpsc::SendError(unsent)| unsent),
    }
}

/// RAII guard ensuring `loop_tx_slot` is cleared even if an ECS system panics.
struct LoopExitGuard {
    loop_tx_slot: Arc<Mutex<Option<SyncSender<EngineCommand>>>>,
}

impl Drop for LoopExitGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.loop_tx_slot.lock() {
            *guard = None;
        }
    }
}

/// Dedicated outbound synchronization worker: applies emitted `GameOutput`
/// batches via `on_outputs` and completes FIFO barriers once synced.
fn run_output_sync_thread<F>(output_rx: mpsc::Receiver<OutputBatch>, mut on_outputs: F)
where
    F: FnMut(Vec<GameOutput>) -> io::Result<()>,
{
    while let Ok(batch) = output_rx.recv() {
        let result = if batch.outputs.is_empty() {
            Ok(())
        } else {
            on_outputs(batch.outputs)
        };
        if let Err(error) = &result {
            tracing::warn!(%error, "failed to sync ECS outputs");
        }
        for barrier in batch.barriers {
            let reply = match &result {
                Ok(()) => Ok(()),
                Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            };
            barrier.send(reply);
        }
    }
}

fn dispatch_tick_batch(
    engine: &mut GameEngine,
    output_tx: &mpsc::Sender<OutputBatch>,
    pending_inputs: &mut Vec<GameInput>,
    pending_barriers: &mut Vec<Reply<io::Result<()>>>,
    extra_barrier: Option<Reply<io::Result<()>>>,
) {
    for input in pending_inputs.drain(..) {
        engine.push_input(input);
    }
    let mut barriers = std::mem::take(pending_barriers);
    if let Some(done) = extra_barrier {
        barriers.push(done);
    }
    let outputs = engine.tick();
    if !outputs.is_empty() || !barriers.is_empty() {
        let _ = output_tx.send(OutputBatch { outputs, barriers });
    }
}

fn drain_shutdown_batch(
    engine: &mut GameEngine,
    output_tx: &mpsc::Sender<OutputBatch>,
    pending_inputs: &mut Vec<GameInput>,
    pending_barriers: &mut Vec<Reply<io::Result<()>>>,
) {
    for input in pending_inputs.drain(..) {
        engine.push_input(input);
    }
    let barriers = std::mem::take(pending_barriers);
    let outputs = engine.drain_inputs_only();
    if !outputs.is_empty() || !barriers.is_empty() {
        let _ = output_tx.send(OutputBatch { outputs, barriers });
    }
}

/// Dedicated 20 TPS fixed-timestep game loop thread.
///
/// Strict fixed-timestep contract:
/// - All state-mutating inputs (`Input`, `InputSync`) are buffered and executed
///   exclusively on 20 TPS tick boundaries inside `GameEngine::tick()`.
/// - Read-only snapshot queries (`PlayerSnapshot`, `PlayerInventory`, `ExistingDrops`)
///   are served immediately between ticks from the last completed tick's authoritative
///   state without flushing or advancing buffered inputs.
/// - Outbound synchronization runs asynchronously on `zcraft-output-sync` so
///   network lock contention never stalls the simulation loop.
#[allow(clippy::too_many_arguments)]
fn run_game_loop_thread(
    mut engine: GameEngine,
    engine_slot: Arc<Mutex<Option<GameEngine>>>,
    loop_tx_slot: Arc<Mutex<Option<SyncSender<EngineCommand>>>>,
    rx: mpsc::Receiver<EngineCommand>,
    output_tx: mpsc::Sender<OutputBatch>,
    mspt_micros: Arc<AtomicU64>,
    tick_notify: Option<tokio::sync::mpsc::Sender<()>>,
) {
    let _exit_guard = LoopExitGuard {
        loop_tx_slot: loop_tx_slot.clone(),
    };
    let mut next_tick = std::time::Instant::now() + world::TICK_INTERVAL;
    let mut shutting_down = false;
    let mut pending_inputs: Vec<GameInput> = Vec::new();
    let mut pending_barriers: Vec<Reply<io::Result<()>>> = Vec::new();
    let mut ema_mspt_us: u64 = 0;
    let mut last_overload_warn: Option<std::time::Instant> = None;

    loop {
        if shutting_down {
            // Drain all remaining queued commands in strict FIFO order before exit.
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    EngineCommand::Input(input) => pending_inputs.push(input),
                    EngineCommand::InputSync { input, done } => {
                        pending_inputs.push(input);
                        pending_barriers.push(done);
                    }
                    EngineCommand::Tick { done } => {
                        dispatch_tick_batch(
                            &mut engine,
                            &output_tx,
                            &mut pending_inputs,
                            &mut pending_barriers,
                            Some(done),
                        );
                    }
                    EngineCommand::PlayerSnapshot { uuid, reply } => {
                        if !pending_inputs.is_empty() || !pending_barriers.is_empty() {
                            drain_shutdown_batch(
                                &mut engine,
                                &output_tx,
                                &mut pending_inputs,
                                &mut pending_barriers,
                            );
                        }
                        reply.send(engine.player_snapshot(uuid));
                    }
                    EngineCommand::PlayerInventory { uuid, reply } => {
                        if !pending_inputs.is_empty() || !pending_barriers.is_empty() {
                            drain_shutdown_batch(
                                &mut engine,
                                &output_tx,
                                &mut pending_inputs,
                                &mut pending_barriers,
                            );
                        }
                        reply.send(engine.player_inventory(uuid));
                    }
                    EngineCommand::ExistingDrops { reply } => {
                        if !pending_inputs.is_empty() || !pending_barriers.is_empty() {
                            drain_shutdown_batch(
                                &mut engine,
                                &output_tx,
                                &mut pending_inputs,
                                &mut pending_barriers,
                            );
                        }
                        reply.send(engine.existing_drops());
                    }
                    EngineCommand::Shutdown => {}
                }
            }
            if !pending_inputs.is_empty() || !pending_barriers.is_empty() {
                drain_shutdown_batch(
                    &mut engine,
                    &output_tx,
                    &mut pending_inputs,
                    &mut pending_barriers,
                );
            }
            break;
        }

        let now_std = std::time::Instant::now();
        if now_std >= next_tick {
            let tick_start = now_std;
            dispatch_tick_batch(
                &mut engine,
                &output_tx,
                &mut pending_inputs,
                &mut pending_barriers,
                None,
            );
            let tick_elapsed = tick_start.elapsed();
            let elapsed_us = tick_elapsed.as_micros() as u64;
            ema_mspt_us = if ema_mspt_us == 0 {
                elapsed_us
            } else {
                (ema_mspt_us * 7 + elapsed_us) / 8
            };
            mspt_micros.store(ema_mspt_us, Ordering::Relaxed);

            if let Some(notify) = &tick_notify {
                let _ = notify.try_send(());
            }
            // Skip missed ticks (matching MissedTickBehavior::Skip) to avoid catch-up spirals.
            let after = std::time::Instant::now();
            next_tick += world::TICK_INTERVAL;
            if next_tick <= after {
                if last_overload_warn
                    .is_none_or(|last| after.duration_since(last) >= OVERLOAD_WARN_INTERVAL)
                {
                    last_overload_warn = Some(after);
                    tracing::warn!(
                        tick_ms = tick_elapsed.as_secs_f64() * 1000.0,
                        ema_mspt_ms = ema_mspt_us as f64 / 1000.0,
                        "Can't keep up! Game tick exceeded 50ms budget"
                    );
                }
                next_tick = after + world::TICK_INTERVAL;
            }
            continue;
        }

        let wait = next_tick.saturating_duration_since(now_std);
        match rx.recv_timeout(wait) {
            Ok(first) => {
                let mut processed = 0usize;
                let mut current = Some(first);
                while let Some(cmd) = current.take() {
                    processed += 1;
                    match cmd {
                        EngineCommand::Input(input) => pending_inputs.push(input),
                        EngineCommand::InputSync { input, done } => {
                            pending_inputs.push(input);
                            pending_barriers.push(done);
                        }
                        EngineCommand::Tick { done } => {
                            dispatch_tick_batch(
                                &mut engine,
                                &output_tx,
                                &mut pending_inputs,
                                &mut pending_barriers,
                                Some(done),
                            );
                        }
                        EngineCommand::PlayerSnapshot { uuid, reply } => {
                            reply.send(engine.player_snapshot(uuid));
                        }
                        EngineCommand::PlayerInventory { uuid, reply } => {
                            reply.send(engine.player_inventory(uuid));
                        }
                        EngineCommand::ExistingDrops { reply } => {
                            reply.send(engine.existing_drops());
                        }
                        EngineCommand::Shutdown => {
                            shutting_down = true;
                            break;
                        }
                    }
                    if processed >= MAX_INPUT_BATCH_PER_TURN
                        || std::time::Instant::now() >= next_tick
                    {
                        break;
                    }
                    match rx.try_recv() {
                        Ok(next) => current = Some(next),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            shutting_down = true;
                            break;
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                shutting_down = true;
            }
        }
    }

    // Restore GameEngine ownership BEFORE dropping `rx` so any concurrent sender
    // observing `Disconnected` is guaranteed to find `Some(engine)` in `engine_slot`.
    *loop_tx_slot.lock().expect("loop_tx poisoned") = None;
    *engine_slot.lock().expect("game engine poisoned") = Some(engine);
    drop(output_tx);
    drop(rx);
}
