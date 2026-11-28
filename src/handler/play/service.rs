//! Shared Play-phase service: hosts the decoupled `bevy_ecs` `GameEngine`,
//! world persistence handle, OP list, command registry, and graceful shutdown trigger.
use super::sync;
use crate::{
    config::GameMode,
    engine::{GameEngine, GameInput, PlayerSnapshot},
    game::Inventory,
    players::Players,
    protocol::{
        Clientbound,
        packets::{
            play::TimeUpdate,
            play_entities::{EntityMetadata, EntityVelocity, SpawnEntity},
        },
    },
    world::{self, World},
};
use std::{
    collections::HashSet,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, AtomicU64, Ordering},
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

/// When fewer than 4ms remain until the next 20 TPS tick, defer non-barrier
/// inputs into the tick boundary instead of running a separate off-tick pass.
const DEFER_NEAR_TICK_WINDOW: Duration = Duration::from_millis(4);

/// When exponential-moving-average tick duration reaches 10ms, adaptively coalesce
/// non-barrier inputs onto tick boundaries to reduce `Players::registry` lock churn.
const DEFER_MSPT_THRESHOLD_US: u64 = 10_000;

/// Minimum interval between slow-tick overload warnings (`Can't keep up!`).
const OVERLOAD_WARN_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) enum Reply<T> {
    Sync(SyncSender<T>),
    Async(tokio::sync::oneshot::Sender<T>),
}

impl<T> Reply<T> {
    fn send(self, value: T) {
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

enum PlayerSaveCommand {
    Save(Box<crate::game::Player>, Box<Inventory>),
    Flush(Reply<()>),
}

pub(crate) enum EngineCommand {
    /// Fire-and-forget gameplay input (movement, digging, placing, inventory, etc.).
    Input(GameInput),
    /// Synchronous/async FIFO barrier input (e.g. PlayerJoin, PlayerLeave, TeleportConfirm)
    /// executed strictly after all preceding queued inputs.
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
        reply: Reply<Vec<crate::engine::DropSnapshot>>,
    },
    /// Graceful shutdown signal: drain remaining queued commands and return `GameEngine`.
    Shutdown,
}

/// Handle owning the dedicated OS game-loop thread and its background player-save worker.
/// Calling `shutdown()` (or dropping the handle) stops the loop cleanly,
/// drains remaining queued commands, joins both threads, and returns `GameEngine`
/// ownership back to `PlayService`.
pub struct GameLoopHandle {
    loop_tx: Arc<Mutex<Option<SyncSender<EngineCommand>>>>,
    player_save_tx: Arc<Mutex<Option<mpsc::Sender<PlayerSaveCommand>>>>,
    tx: Option<SyncSender<EngineCommand>>,
    thread: Option<JoinHandle<()>>,
    save_thread: Option<JoinHandle<()>>,
}

impl GameLoopHandle {
    /// Gracefully stop the dedicated game-loop thread, draining queued commands,
    /// flushing all pending playerdata saves, and returning `GameEngine` ownership.
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
        // 2. Game loop has drained all PlayerLeave events and dropped its save sender.
        //    Detach the shared save sender and wait for all pending playerdata writes.
        *self.player_save_tx.lock().expect("player_save_tx poisoned") = None;
        if let Some(handle) = self.save_thread.take()
            && let Err(error) = handle.join()
        {
            tracing::error!(?error, "player persistence worker thread panicked");
        }
    }
}

impl Drop for GameLoopHandle {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

pub struct PlayService {
    world: Arc<World>,
    /// Holds `Some(GameEngine)` when running headless (no dedicated game loop
    /// attached) and `None` while the dedicated OS game-loop thread exclusively
    /// owns the engine. Ownership is restored on clean loop shutdown.
    engine: Arc<Mutex<Option<GameEngine>>>,
    /// Bounded FIFO channel to the dedicated OS game-loop thread when active.
    loop_tx: Arc<Mutex<Option<SyncSender<EngineCommand>>>>,
    /// Background playerdata persistence worker channel (off the game-loop thread).
    player_save_tx: Arc<Mutex<Option<mpsc::Sender<PlayerSaveCommand>>>>,
    /// Exponential-moving-average tick duration in microseconds (MSPT * 1000).
    mspt_micros: Arc<AtomicU64>,
    ops: Mutex<HashSet<String>>,
    shutdown: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    pub(super) game_mode: GameMode,
    commands: crate::command::CommandRegistry,
    max_view_distance: i32,
    next_entity: Arc<AtomicI32>,
}

impl PlayService {
    pub fn new() -> io::Result<Self> {
        Self::with_max_view_distance(world::DEFAULT_VIEW_DISTANCE)
    }

    pub fn with_max_view_distance(max_view_distance: i32) -> io::Result<Self> {
        Self::with_options(max_view_distance, GameMode::default())
    }

    pub fn with_options(max_view_distance: i32, game_mode: GameMode) -> io::Result<Self> {
        Self::with_world(max_view_distance, game_mode, Arc::new(World::new()))
    }

    pub fn with_world(
        max_view_distance: i32,
        game_mode: GameMode,
        world: Arc<World>,
    ) -> io::Result<Self> {
        world::validate_view_distance(max_view_distance)?;
        let next_entity = Arc::new(AtomicI32::new(1));
        let engine = GameEngine::new(world.clone(), next_entity.clone());
        Ok(Self {
            world,
            engine: Arc::new(Mutex::new(Some(engine))),
            loop_tx: Arc::new(Mutex::new(None)),
            player_save_tx: Arc::new(Mutex::new(None)),
            mspt_micros: Arc::new(AtomicU64::new(0)),
            ops: Mutex::new(HashSet::new()),
            shutdown: Mutex::new(None),
            game_mode,
            commands: crate::command::CommandRegistry::default(),
            max_view_distance,
            next_entity,
        })
    }

    /// Move `GameEngine` into a dedicated OS thread with a bounded FIFO command
    /// queue and an independent 20 TPS tick loop so Tokio async IO workers never
    /// contend on `GameEngine` or execute gameplay simulation.
    pub(crate) fn start_game_loop(
        &self,
        players: Players,
        tick_notify: Option<tokio::sync::mpsc::Sender<()>>,
    ) -> io::Result<GameLoopHandle> {
        self.start_game_loop_with_capacity(players, GAME_INPUT_QUEUE_CAPACITY, tick_notify)
    }

    pub(crate) fn start_game_loop_with_capacity(
        &self,
        players: Players,
        capacity: usize,
        tick_notify: Option<tokio::sync::mpsc::Sender<()>>,
    ) -> io::Result<GameLoopHandle> {
        let engine = self
            .engine
            .lock()
            .expect("game engine poisoned")
            .take()
            .ok_or_else(|| io::Error::other("game loop is already running"))?;

        let (save_tx, save_rx) = mpsc::channel::<PlayerSaveCommand>();
        let save_world = self.world.clone();
        let save_thread = thread::Builder::new()
            .name("zcraft-player-save".into())
            .spawn(move || {
                while let Ok(cmd) = save_rx.recv() {
                    match cmd {
                        PlayerSaveCommand::Save(player, inventory) => {
                            if let Err(error) = save_world.save_player(&player, &inventory) {
                                tracing::warn!(
                                    uuid = %player.uuid,
                                    %error,
                                    "failed to save playerdata"
                                );
                            }
                        }
                        PlayerSaveCommand::Flush(done) => {
                            done.send(());
                        }
                    }
                }
            })?;
        *self.player_save_tx.lock().expect("player_save_tx poisoned") = Some(save_tx.clone());

        let (tx, rx) = mpsc::sync_channel(capacity.max(1));
        *self.loop_tx.lock().expect("loop_tx poisoned") = Some(tx.clone());

        let engine_slot = self.engine.clone();
        let loop_tx_slot = self.loop_tx.clone();
        let mspt_micros = self.mspt_micros.clone();
        let thread = match thread::Builder::new()
            .name("zcraft-game-loop".into())
            .spawn(move || {
                run_game_loop_thread(
                    engine,
                    engine_slot,
                    loop_tx_slot,
                    rx,
                    players,
                    save_tx,
                    mspt_micros,
                    tick_notify,
                );
            }) {
            Ok(handle) => handle,
            Err(error) => {
                *self.loop_tx.lock().expect("loop_tx poisoned") = None;
                *self.player_save_tx.lock().expect("player_save_tx poisoned") = None;
                let _ = save_thread.join();
                return Err(error);
            }
        };

        Ok(GameLoopHandle {
            loop_tx: self.loop_tx.clone(),
            player_save_tx: self.player_save_tx.clone(),
            tx: Some(tx),
            thread: Some(thread),
            save_thread: Some(save_thread),
        })
    }

    fn current_loop_tx(&self) -> Option<SyncSender<EngineCommand>> {
        self.loop_tx.lock().expect("loop_tx poisoned").clone()
    }

    /// Wait for any queued background playerdata writes to hit disk before loading.
    pub(crate) fn flush_player_saves(&self) {
        let tx = self
            .player_save_tx
            .lock()
            .expect("player_save_tx poisoned")
            .clone();
        if let Some(tx) = tx {
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            if tx
                .send(PlayerSaveCommand::Flush(Reply::Sync(done_tx)))
                .is_ok()
            {
                let _ = done_rx.recv();
            }
        }
    }

    /// Non-blocking async barrier waiting for queued background playerdata writes.
    pub(crate) async fn flush_player_saves_async(&self) {
        let tx = self
            .player_save_tx
            .lock()
            .expect("player_save_tx poisoned")
            .clone();
        if let Some(tx) = tx {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            if tx
                .send(PlayerSaveCommand::Flush(Reply::Async(done_tx)))
                .is_ok()
            {
                let _ = done_rx.await;
            }
        }
    }

    fn with_inline_engine<R>(&self, f: impl FnOnce(&mut GameEngine) -> R) -> io::Result<R> {
        let mut guard = self.engine.lock().expect("game engine poisoned");
        let engine = guard
            .as_mut()
            .ok_or_else(|| io::Error::other("game engine unavailable"))?;
        Ok(f(engine))
    }

    fn persist_inline(&self, player: Box<crate::game::Player>, inventory: Box<Inventory>) {
        if let Err(error) = self.world.save_player(&player, &inventory) {
            tracing::warn!(
                uuid = %player.uuid,
                %error,
                "failed to save playerdata"
            );
        }
    }

    /// Submit a fire-and-forget gameplay input to the bounded FIFO game-loop queue.
    /// Returns `WouldBlock` when the queue is full (inbound backpressure), or falls
    /// back to inline execution when running headless without a game loop.
    pub(crate) fn dispatch_input(&self, input: GameInput, players: &Players) -> io::Result<()> {
        let pending = if let Some(tx) = self.current_loop_tx() {
            match tx.try_send(EngineCommand::Input(input)) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "game engine input queue full",
                    ));
                }
                Err(TrySendError::Disconnected(EngineCommand::Input(unsent))) => unsent,
                Err(TrySendError::Disconnected(_)) => unreachable!(),
            }
        } else {
            input
        };
        self.submit_input_inline(pending, players)
    }

    /// Submit a gameplay input with a synchronous FIFO completion barrier (used for
    /// Drop guards, commands, and headless tests).
    pub(crate) fn submit_input_sync(&self, input: GameInput, players: &Players) -> io::Result<()> {
        let pending = if let Some(tx) = self.current_loop_tx() {
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            match tx.send(EngineCommand::InputSync {
                input,
                done: Reply::Sync(done_tx),
            }) {
                Ok(()) => {
                    return done_rx
                        .recv()
                        .unwrap_or_else(|_| Err(io::Error::other("game loop terminated")));
                }
                Err(mpsc::SendError(EngineCommand::InputSync { input: unsent, .. })) => unsent,
                Err(mpsc::SendError(_)) => unreachable!(),
            }
        } else {
            input
        };
        self.submit_input_inline(pending, players)
    }

    /// Async FIFO completion barrier using `tokio::sync::oneshot` so Tokio async
    /// workers never block waiting on the game-loop thread.
    pub(crate) async fn submit_input_async(
        &self,
        input: GameInput,
        players: &Players,
    ) -> io::Result<()> {
        let pending = if let Some(tx) = self.current_loop_tx() {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let cmd = EngineCommand::InputSync {
                input,
                done: Reply::Async(done_tx),
            };
            match send_command_async(tx, cmd).await {
                Ok(()) => {
                    return done_rx
                        .await
                        .unwrap_or_else(|_| Err(io::Error::other("game loop terminated")));
                }
                Err(EngineCommand::InputSync { input: unsent, .. }) => unsent,
                Err(_) => unreachable!(),
            }
        } else {
            input
        };
        self.submit_input_inline(pending, players)
    }

    /// Latest exponential-moving-average tick duration in microseconds (MSPT * 1000).
    pub fn mspt_micros(&self) -> u64 {
        self.mspt_micros.load(Ordering::Relaxed)
    }

    fn submit_input_inline(&self, input: GameInput, players: &Players) -> io::Result<()> {
        let outputs = self.with_inline_engine(|engine| {
            engine.push_input(input);
            engine.process_inputs()
        })?;
        sync::apply_outputs(players, outputs, |p, inv| self.persist_inline(p, inv))
    }

    /// Advance the entire `bevy_ecs` game simulation by one 20 TPS tick and
    /// synchronize emitted state changes to `players`.
    pub(crate) fn tick_engine(&self, players: &Players) -> io::Result<()> {
        if let Some(tx) = self.current_loop_tx() {
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            if tx
                .send(EngineCommand::Tick {
                    done: Reply::Sync(done_tx),
                })
                .is_ok()
            {
                return done_rx
                    .recv()
                    .unwrap_or_else(|_| Err(io::Error::other("game loop terminated")));
            }
        }
        let outputs = self.with_inline_engine(GameEngine::tick)?;
        sync::apply_outputs(players, outputs, |p, inv| self.persist_inline(p, inv))
    }

    /// Read-only snapshot of an active player entity from the ECS world.
    pub fn player_snapshot(&self, uuid: Uuid) -> Option<PlayerSnapshot> {
        if let Some(tx) = self.current_loop_tx() {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            if tx
                .send(EngineCommand::PlayerSnapshot {
                    uuid,
                    reply: Reply::Sync(reply_tx),
                })
                .is_ok()
            {
                return reply_rx.recv().ok().flatten();
            }
        }
        self.engine
            .lock()
            .expect("game engine poisoned")
            .as_ref()
            .and_then(|engine| engine.player_snapshot(uuid))
    }

    /// Non-blocking async snapshot query for Tokio handlers.
    pub async fn player_snapshot_async(&self, uuid: Uuid) -> Option<PlayerSnapshot> {
        if let Some(tx) = self.current_loop_tx() {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let cmd = EngineCommand::PlayerSnapshot {
                uuid,
                reply: Reply::Async(reply_tx),
            };
            if send_command_async(tx, cmd).await.is_ok() {
                return reply_rx.await.ok().flatten();
            }
        }
        self.engine
            .lock()
            .expect("game engine poisoned")
            .as_ref()
            .and_then(|engine| engine.player_snapshot(uuid))
    }

    /// Clone a player's current authoritative inventory from the ECS world.
    pub fn player_inventory(&self, uuid: Uuid) -> Option<Inventory> {
        if let Some(tx) = self.current_loop_tx() {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            if tx
                .send(EngineCommand::PlayerInventory {
                    uuid,
                    reply: Reply::Sync(reply_tx),
                })
                .is_ok()
            {
                return reply_rx.recv().ok().flatten();
            }
        }
        self.engine
            .lock()
            .expect("game engine poisoned")
            .as_ref()
            .and_then(|engine| engine.player_inventory(uuid))
    }

    /// Non-blocking async inventory query for Tokio handlers.
    pub async fn player_inventory_async(&self, uuid: Uuid) -> Option<Inventory> {
        if let Some(tx) = self.current_loop_tx() {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let cmd = EngineCommand::PlayerInventory {
                uuid,
                reply: Reply::Async(reply_tx),
            };
            if send_command_async(tx, cmd).await.is_ok() {
                return reply_rx.await.ok().flatten();
            }
        }
        self.engine
            .lock()
            .expect("game engine poisoned")
            .as_ref()
            .and_then(|engine| engine.player_inventory(uuid))
    }

    pub fn world(&self) -> &Arc<World> {
        &self.world
    }

    pub fn save_world(&self) -> io::Result<world::SaveSummary> {
        self.world.save_all()
    }

    pub fn load_player(&self, uuid: Uuid) -> io::Result<Option<world::SavedPlayer>> {
        self.flush_player_saves();
        self.world.load_player(uuid)
    }

    pub fn save_player(
        &self,
        player: &crate::game::Player,
        inventory: &crate::game::Inventory,
    ) -> io::Result<()> {
        self.world.save_player(player, inventory)
    }

    pub(super) fn allocate_entity_id(&self) -> io::Result<i32> {
        self.next_entity
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| io::Error::other("entity ID space exhausted"))
    }

    pub fn with_ops(self, ops: impl IntoIterator<Item = String>) -> Self {
        {
            let mut set = self.ops.lock().expect("ops poisoned");
            for entry in ops {
                set.insert(normalize_op(&entry));
            }
        }
        self
    }

    pub fn permission_for(&self, uuid: Uuid, username: &str) -> u8 {
        let ops = self.ops.lock().expect("ops poisoned");
        if ops.contains(&uuid.to_string()) || ops.contains(&username.to_ascii_lowercase()) {
            4
        } else {
            0
        }
    }

    pub fn add_op(&self, target: &str) -> bool {
        self.ops
            .lock()
            .expect("ops poisoned")
            .insert(normalize_op(target))
    }

    pub fn remove_op(&self, target: &str) -> bool {
        self.ops
            .lock()
            .expect("ops poisoned")
            .remove(&normalize_op(target))
    }

    pub fn set_shutdown_trigger(&self, tx: tokio::sync::watch::Sender<bool>) {
        *self.shutdown.lock().expect("shutdown poisoned") = Some(tx);
    }

    pub fn request_shutdown(&self) -> bool {
        if let Some(tx) = self.shutdown.lock().expect("shutdown poisoned").as_ref() {
            let _ = tx.send(true);
            true
        } else {
            false
        }
    }

    pub fn set_block(
        &self,
        players: &Players,
        pos: world::BlockPos,
        state: i32,
    ) -> io::Result<bool> {
        let Some(mutation) = self.world.set_block(pos, state) else {
            return Ok(false);
        };
        sync::publish_mutation(players, mutation)?;
        Ok(true)
    }

    pub fn fill_blocks(
        &self,
        players: &Players,
        from: world::BlockPos,
        to: world::BlockPos,
        state: i32,
    ) -> io::Result<usize> {
        let mutation = self.world.fill_blocks(from, to, state)?;
        let count = mutation.block_changes.len();
        sync::publish_mutation(players, mutation)?;
        Ok(count)
    }

    /// Build spawn/metadata/velocity packets for all active item drop entities.
    pub fn existing_drop_packets(&self) -> Vec<Clientbound> {
        let drops = if let Some(tx) = self.current_loop_tx() {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            if tx
                .send(EngineCommand::ExistingDrops {
                    reply: Reply::Sync(reply_tx),
                })
                .is_ok()
            {
                reply_rx.recv().unwrap_or_default()
            } else {
                self.engine
                    .lock()
                    .expect("game engine poisoned")
                    .as_mut()
                    .map_or_else(Vec::new, GameEngine::existing_drops)
            }
        } else {
            self.engine
                .lock()
                .expect("game engine poisoned")
                .as_mut()
                .map_or_else(Vec::new, GameEngine::existing_drops)
        };
        encode_drop_packets(drops)
    }

    /// Non-blocking async variant of `existing_drop_packets` for Tokio handlers.
    pub async fn existing_drop_packets_async(&self) -> Vec<Clientbound> {
        let drops = if let Some(tx) = self.current_loop_tx() {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let cmd = EngineCommand::ExistingDrops {
                reply: Reply::Async(reply_tx),
            };
            if send_command_async(tx, cmd).await.is_ok() {
                reply_rx.await.unwrap_or_default()
            } else {
                self.engine
                    .lock()
                    .expect("game engine poisoned")
                    .as_mut()
                    .map_or_else(Vec::new, GameEngine::existing_drops)
            }
        } else {
            self.engine
                .lock()
                .expect("game engine poisoned")
                .as_mut()
                .map_or_else(Vec::new, GameEngine::existing_drops)
        };
        encode_drop_packets(drops)
    }

    pub fn world_time(&self) -> (i64, i64) {
        let time = self.world.time();
        (time.world_age, time.day_time)
    }

    pub fn weather(&self) -> world::WeatherSnapshot {
        self.world.weather()
    }

    pub fn set_day_time(&self, day_time: i64) -> TimeUpdate {
        sync::encode_time(self.world.set_day_time(day_time))
    }

    pub fn set_weather(&self, raining: bool, thundering: bool, duration: u32) -> Vec<Clientbound> {
        self.world
            .set_weather(raining, thundering, duration)
            .into_iter()
            .map(sync::encode_weather)
            .collect()
    }

    pub(super) fn initial_environment_packets(&self) -> Vec<Clientbound> {
        let (time, weather) = self.world.initial_environment();
        let mut packets = Vec::with_capacity(weather.len() + 1);
        packets.push(sync::encode_time(time).into());
        packets.extend(weather.into_iter().map(sync::encode_weather));
        packets
    }

    /// Install a validated immutable registry before serving connections.
    pub fn with_commands(mut self, commands: crate::command::CommandRegistry) -> Self {
        self.commands = commands;
        self
    }

    pub fn commands(&self) -> &crate::command::CommandRegistry {
        &self.commands
    }

    pub fn game_mode(&self) -> GameMode {
        self.game_mode
    }

    pub fn max_view_distance(&self) -> i32 {
        self.max_view_distance
    }

    pub(super) fn view_distance(
        &self,
        information: Option<&crate::protocol::packets::common::ClientInformation>,
    ) -> i32 {
        information.map_or(self.max_view_distance, |info| {
            i32::from(info.view_distance).clamp(world::VIEW_DISTANCE, self.max_view_distance)
        })
    }
}

fn normalize_op(entry: &str) -> String {
    if let Ok(uuid) = Uuid::parse_str(entry) {
        uuid.to_string()
    } else {
        entry.to_ascii_lowercase()
    }
}

fn encode_drop_packets(drops: Vec<crate::engine::DropSnapshot>) -> Vec<Clientbound> {
    let mut packets = Vec::with_capacity(drops.len() * 3);
    for drop in drops {
        packets.push(SpawnEntity::item(drop.id, drop.uuid, drop.position, drop.velocity).into());
        packets.push(
            EntityMetadata::Item {
                id: drop.id,
                stack: drop.stack,
            }
            .into(),
        );
        packets.push(EntityVelocity::new(drop.id, drop.velocity).into());
    }
    packets
}

/// Non-blocking send onto the bounded `EngineCommand` channel for Tokio callers:
/// uses fast-path `try_send` and only offloads to `spawn_blocking` when full.
async fn send_command_async(
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

fn enqueue_player_save(
    save_tx: &mpsc::Sender<PlayerSaveCommand>,
    player: Box<crate::game::Player>,
    inventory: Box<Inventory>,
) {
    if save_tx
        .send(PlayerSaveCommand::Save(player, inventory))
        .is_err()
    {
        tracing::error!("player persistence worker channel closed; playerdata not saved");
    }
}

/// Decide dynamically whether non-barrier inputs should defer to the upcoming
/// 20 TPS tick boundary instead of executing in an immediate off-tick pass.
fn should_defer_inputs(
    remaining_until_tick: Duration,
    ema_mspt_us: u64,
    buffered_len: usize,
) -> bool {
    remaining_until_tick <= DEFER_NEAR_TICK_WINDOW
        || ema_mspt_us >= DEFER_MSPT_THRESHOLD_US
        || buffered_len >= MAX_INPUT_BATCH_PER_TURN
}

fn flush_inputs(
    engine: &mut GameEngine,
    players: &Players,
    save_tx: &mpsc::Sender<PlayerSaveCommand>,
    pending: &mut Vec<GameInput>,
) {
    for input in pending.drain(..) {
        engine.push_input(input);
    }
    let outputs = engine.process_inputs();
    if let Err(error) = sync::apply_outputs(players, outputs, |p, inv| {
        enqueue_player_save(save_tx, p, inv)
    }) {
        tracing::warn!(%error, "failed to sync ECS input batch");
    }
}

fn execute_command(
    engine: &mut GameEngine,
    players: &Players,
    save_tx: &mpsc::Sender<PlayerSaveCommand>,
    cmd: EngineCommand,
) {
    match cmd {
        EngineCommand::Input(input) => {
            engine.push_input(input);
            let outputs = engine.process_inputs();
            if let Err(error) = sync::apply_outputs(players, outputs, |p, inv| {
                enqueue_player_save(save_tx, p, inv)
            }) {
                tracing::warn!(%error, "failed to sync ECS input");
            }
        }
        EngineCommand::InputSync { input, done } => {
            engine.push_input(input);
            let outputs = engine.process_inputs();
            let result = sync::apply_outputs(players, outputs, |p, inv| {
                enqueue_player_save(save_tx, p, inv)
            });
            done.send(result);
        }
        EngineCommand::Tick { done } => {
            let outputs = engine.tick();
            let result = sync::apply_outputs(players, outputs, |p, inv| {
                enqueue_player_save(save_tx, p, inv)
            });
            done.send(result);
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
        EngineCommand::Shutdown => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn run_game_loop_thread(
    mut engine: GameEngine,
    engine_slot: Arc<Mutex<Option<GameEngine>>>,
    loop_tx_slot: Arc<Mutex<Option<SyncSender<EngineCommand>>>>,
    rx: mpsc::Receiver<EngineCommand>,
    players: Players,
    save_tx: mpsc::Sender<PlayerSaveCommand>,
    mspt_micros: Arc<AtomicU64>,
    tick_notify: Option<tokio::sync::mpsc::Sender<()>>,
) {
    let _exit_guard = LoopExitGuard {
        loop_tx_slot: loop_tx_slot.clone(),
    };
    let mut next_tick = std::time::Instant::now() + world::TICK_INTERVAL;
    let mut shutting_down = false;
    let mut deferred_inputs: Vec<GameInput> = Vec::new();
    let mut ema_mspt_us: u64 = 0;
    let mut last_overload_warn: Option<std::time::Instant> = None;

    loop {
        if shutting_down {
            // Drain all remaining queued commands in strict FIFO order before exit.
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    EngineCommand::Input(input) => deferred_inputs.push(input),
                    EngineCommand::Shutdown => {}
                    other => {
                        if !deferred_inputs.is_empty() {
                            flush_inputs(&mut engine, &players, &save_tx, &mut deferred_inputs);
                        }
                        execute_command(&mut engine, &players, &save_tx, other);
                    }
                }
            }
            if !deferred_inputs.is_empty() {
                flush_inputs(&mut engine, &players, &save_tx, &mut deferred_inputs);
            }
            break;
        }

        let now_std = std::time::Instant::now();
        if now_std >= next_tick {
            let tick_start = now_std;
            for input in deferred_inputs.drain(..) {
                engine.push_input(input);
            }
            let outputs = engine.tick();
            if let Err(error) = sync::apply_outputs(&players, outputs, |p, inv| {
                enqueue_player_save(&save_tx, p, inv)
            }) {
                tracing::warn!(%error, "failed to sync ECS game tick outputs");
            }
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
                        EngineCommand::Input(input) => deferred_inputs.push(input),
                        EngineCommand::Shutdown => {
                            shutting_down = true;
                            break;
                        }
                        other => {
                            // Always flush buffered inputs before barriers/queries for strict FIFO.
                            if !deferred_inputs.is_empty() {
                                flush_inputs(&mut engine, &players, &save_tx, &mut deferred_inputs);
                            }
                            execute_command(&mut engine, &players, &save_tx, other);
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
                // Dynamically decide whether to flush immediately (low latency) or
                // keep buffered until the upcoming 20 TPS tick boundary (coalesced).
                if !deferred_inputs.is_empty() {
                    let remaining = next_tick.saturating_duration_since(std::time::Instant::now());
                    if !should_defer_inputs(remaining, ema_mspt_us, deferred_inputs.len()) {
                        flush_inputs(&mut engine, &players, &save_tx, &mut deferred_inputs);
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
    drop(save_tx);
    drop(rx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Item, Player};

    #[test]
    fn fifo_ordering_prevents_leave_from_overtaking_queued_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let world = Arc::new(World::open(dir.path(), None).unwrap());
        let service = PlayService::with_world(2, GameMode::Creative, world).unwrap();
        let players = Players::new(4);
        let handle = service.start_game_loop(players.clone(), None).unwrap();

        let uuid = Uuid::from_u128(1001);
        let player = Player::at_spawn(1, uuid, "Steve".into(), GameMode::Creative);
        service
            .submit_input_sync(
                GameInput::PlayerJoin {
                    player: Box::new(player),
                    inventory: Box::new(Inventory::default()),
                },
                &players,
            )
            .unwrap();

        // Queue several fire-and-forget inputs followed immediately by a synchronous
        // PlayerLeave barrier. FIFO ordering guarantees the diamond is given and
        // persisted before PlayerLeave despawns the entity.
        let diamond = Item::by_name("diamond").unwrap();
        service
            .dispatch_input(
                GameInput::ControlGive {
                    uuid,
                    item: diamond,
                    count: 5,
                },
                &players,
            )
            .unwrap();
        service
            .submit_input_sync(
                GameInput::PlayerLeave {
                    uuid,
                    persist: true,
                },
                &players,
            )
            .unwrap();

        handle.shutdown();

        let saved = service
            .load_player(uuid)
            .unwrap()
            .expect("playerdata persisted");
        let diamond_count: u32 = saved
            .inventory
            .slots()
            .iter()
            .flatten()
            .filter(|stack| stack.item == diamond)
            .map(|stack| u32::from(stack.count))
            .sum();
        assert_eq!(diamond_count, 5);
    }

    #[test]
    fn bounded_queue_applies_backpressure_with_would_block() {
        let service = PlayService::new().unwrap();
        let players = Players::new(4);
        let (tx, _rx) = mpsc::sync_channel::<EngineCommand>(2);
        *service.loop_tx.lock().unwrap() = Some(tx);

        let uuid = Uuid::from_u128(2002);
        for _ in 0..2 {
            service
                .dispatch_input(GameInput::PlayerSelectSlot { uuid, slot: 0 }, &players)
                .unwrap();
        }
        let err = service
            .dispatch_input(GameInput::PlayerSelectSlot { uuid, slot: 1 }, &players)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn shutdown_drains_commands_and_restores_headless_engine_ownership() {
        let service = PlayService::new().unwrap();
        let players = Players::new(4);
        let handle = service.start_game_loop(players.clone(), None).unwrap();

        let uuid = Uuid::from_u128(3003);
        let player = Player::at_spawn(1, uuid, "Alex".into(), GameMode::Survival);
        service
            .submit_input_sync(
                GameInput::PlayerJoin {
                    player: Box::new(player),
                    inventory: Box::new(Inventory::default()),
                },
                &players,
            )
            .unwrap();

        // Queries work lock-free while the dedicated OS thread owns GameEngine.
        let snap = service.player_snapshot(uuid).expect("online snapshot");
        assert_eq!(snap.name, "Alex");
        assert!(service.player_inventory(uuid).is_some());
        assert!(service.existing_drop_packets().is_empty());

        // Shut down the dedicated thread and verify GameEngine ownership returns.
        handle.shutdown();
        let snap_after = service
            .player_snapshot(uuid)
            .expect("headless snapshot after shutdown");
        assert_eq!(snap_after.name, "Alex");
        service.tick_engine(&players).unwrap();
    }

    #[test]
    fn dedicated_loop_advances_periodic_ticks_independently() {
        let service = PlayService::new().unwrap();
        let players = Players::new(4);
        let (tick_tx, mut tick_rx) = tokio::sync::mpsc::channel(4);
        let handle = service
            .start_game_loop(players.clone(), Some(tick_tx))
            .unwrap();

        let start_age = service.world_time().0;
        // Wait for at least two 20 TPS ticks (~100ms) from the dedicated OS thread.
        assert!(tick_rx.blocking_recv().is_some());
        assert!(tick_rx.blocking_recv().is_some());
        let end_age = service.world_time().0;
        assert!(end_age >= start_age + 2);

        handle.shutdown();
    }

    #[test]
    fn adaptive_input_deferral_switches_by_deadline_load_and_batch_size() {
        // Light load and plenty of time before next tick -> execute immediately.
        assert!(!should_defer_inputs(Duration::from_millis(20), 2_000, 10));
        // Near tick boundary (<= 4ms) -> defer to tick boundary.
        assert!(should_defer_inputs(Duration::from_millis(3), 2_000, 1));
        // High MSPT (>= 10ms) -> defer to coalesce onto tick boundary.
        assert!(should_defer_inputs(Duration::from_millis(20), 12_000, 1));
        // Burst reached batch cap -> defer remaining.
        assert!(should_defer_inputs(
            Duration::from_millis(20),
            2_000,
            MAX_INPUT_BATCH_PER_TURN
        ));
    }
}
