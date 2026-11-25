//! Minimal flat world with spectator and server-authoritative creative modes.
pub(crate) mod blocks;
pub(crate) mod chat;
mod chunks;

use self::chunks::ChunkStream;
use super::{Context, Outcome, Session};
use crate::{
    config::GameMode,
    players::Peer,
    protocol::{
        Clientbound, invalid,
        packets::{
            play::*,
            play_blocks::{InventoryContent, SelectedSlot},
            play_chat::DeclareCommands,
            play_common::*,
            play_entities::EntityPose,
        },
    },
    world,
};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;

const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const TELEPORT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct PlayService {
    terrain: Arc<[u8]>,
    blocks: Arc<world::BlockWorld>,
    clock: std::sync::Mutex<world::WorldClock>,
    ops: std::sync::Mutex<std::collections::HashSet<String>>,
    shutdown: std::sync::Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    game_mode: GameMode,
    commands: crate::command::CommandRegistry,
    max_view_distance: i32,
    next_entity: AtomicI32,
}
impl PlayService {
    pub fn new() -> io::Result<Self> {
        Self::with_max_view_distance(world::VIEW_DISTANCE)
    }

    pub fn with_max_view_distance(max_view_distance: i32) -> io::Result<Self> {
        Self::with_options(max_view_distance, GameMode::Spectator)
    }

    pub fn with_options(max_view_distance: i32, game_mode: GameMode) -> io::Result<Self> {
        world::validate_view_distance(max_view_distance)?;
        Ok(Self {
            terrain: world::chunk_payload(),
            blocks: Arc::default(),
            clock: std::sync::Mutex::new(world::WorldClock::new()),
            ops: std::sync::Mutex::new(std::collections::HashSet::new()),
            shutdown: std::sync::Mutex::new(None),
            game_mode,
            commands: crate::command::CommandRegistry::default(),
            max_view_distance,
            next_entity: AtomicI32::new(1),
        })
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

    pub fn permission_for(&self, uuid: uuid::Uuid, username: &str) -> u8 {
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
        players: &crate::players::Players,
        pos: world::BlockPos,
        state: i32,
    ) -> io::Result<bool> {
        if !pos.valid() {
            return Ok(false);
        }
        let mut blocks = self.blocks.lock();
        let chunk = pos.chunk();
        let affected = world::affected_light_chunks(pos);
        let old_lights: Vec<_> = affected
            .iter()
            .map(|&nc| (nc, world::chunk_light(&blocks.neighborhood(nc))))
            .collect();
        if !blocks.set(pos, state) {
            return Ok(false);
        }
        for (nc, old_light) in old_lights {
            let new_light = world::chunk_light(&blocks.neighborhood(nc));
            let mut bodies = Vec::new();
            if nc == chunk {
                bodies.push(
                    Clientbound::from(crate::protocol::packets::play_blocks::BlockUpdate(
                        pos, state,
                    ))
                    .encode()?
                    .into(),
                );
            }
            if new_light != old_light {
                bodies.push(
                    Clientbound::from(crate::protocol::packets::play_blocks::LightUpdate(
                        nc, new_light,
                    ))
                    .encode()?
                    .into(),
                );
            }
            if !bodies.is_empty() {
                players.publish_world_updates(nc, bodies.into());
            }
        }
        Ok(true)
    }

    pub fn fill_blocks(
        &self,
        players: &crate::players::Players,
        from: world::BlockPos,
        to: world::BlockPos,
        state: i32,
    ) -> io::Result<usize> {
        const MAX_FILL_BLOCKS: i64 = 4096;
        let min_x = from.0[0].min(to.0[0]);
        let max_x = from.0[0].max(to.0[0]);
        let min_y = from.0[1].min(to.0[1]);
        let max_y = from.0[1].max(to.0[1]);
        let min_z = from.0[2].min(to.0[2]);
        let max_z = from.0[2].max(to.0[2]);
        let start = world::BlockPos([min_x, min_y, min_z]);
        let end = world::BlockPos([max_x, max_y, max_z]);
        if !start.valid() || !end.valid() {
            return Err(invalid("fill coordinates are out of world bounds"));
        }
        let volume = i64::from(max_x - min_x + 1)
            * i64::from(max_y - min_y + 1)
            * i64::from(max_z - min_z + 1);
        if volume > MAX_FILL_BLOCKS {
            return Err(invalid("fill volume exceeds 4096 blocks"));
        }

        let mut blocks = self.blocks.lock();
        let mut light_chunks = std::collections::BTreeSet::new();
        let mut per_chunk_updates: std::collections::BTreeMap<[i32; 2], Vec<Arc<[u8]>>> =
            std::collections::BTreeMap::new();
        for x in min_x..=max_x {
            for z in min_z..=max_z {
                for nc in world::affected_light_chunks(world::BlockPos([x, min_y, z])) {
                    light_chunks.insert(nc);
                }
            }
        }
        let old_lights: Vec<_> = light_chunks
            .iter()
            .map(|&nc| (nc, world::chunk_light(&blocks.neighborhood(nc))))
            .collect();

        let mut changed = 0usize;
        for y in min_y..=max_y {
            for z in min_z..=max_z {
                for x in min_x..=max_x {
                    let pos = world::BlockPos([x, y, z]);
                    if blocks.get(pos) == state {
                        continue;
                    }
                    if blocks.set(pos, state) {
                        changed += 1;
                        let body = Clientbound::from(
                            crate::protocol::packets::play_blocks::BlockUpdate(pos, state),
                        )
                        .encode()?
                        .into();
                        per_chunk_updates.entry(pos.chunk()).or_default().push(body);
                    }
                }
            }
        }
        for (nc, old_light) in old_lights {
            let new_light = world::chunk_light(&blocks.neighborhood(nc));
            if new_light != old_light {
                let body = Clientbound::from(crate::protocol::packets::play_blocks::LightUpdate(
                    nc, new_light,
                ))
                .encode()?
                .into();
                per_chunk_updates.entry(nc).or_default().push(body);
            }
        }
        for (nc, bodies) in per_chunk_updates {
            players.publish_world_updates(nc, bodies.into());
        }
        Ok(changed)
    }

    /// Advance the world clock and weather cycle by one 20 TPS tick.
    pub fn tick_world(&self) -> Vec<Clientbound> {
        self.clock.lock().expect("world clock poisoned").tick()
    }

    pub fn world_time(&self) -> (i64, i64) {
        let clock = self.clock.lock().expect("world clock poisoned");
        (clock.world_age(), clock.day_time())
    }

    pub fn weather(&self) -> world::WeatherSnapshot {
        self.clock.lock().expect("world clock poisoned").weather()
    }

    pub fn set_day_time(&self, day_time: i64) -> TimeUpdate {
        self.clock
            .lock()
            .expect("world clock poisoned")
            .set_day_time(day_time)
    }

    pub fn set_weather(&self, raining: bool, thundering: bool, duration: u32) -> Vec<Clientbound> {
        self.clock
            .lock()
            .expect("world clock poisoned")
            .set_weather(raining, thundering, duration)
    }

    fn initial_environment_packets(&self) -> Vec<Clientbound> {
        self.clock
            .lock()
            .expect("world clock poisoned")
            .initial_packets()
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

    fn view_distance(
        &self,
        information: Option<&crate::protocol::packets::common::ClientInformation>,
    ) -> i32 {
        information.map_or(self.max_view_distance, |info| {
            i32::from(info.view_distance).clamp(world::VIEW_DISTANCE, self.max_view_distance)
        })
    }
}

#[derive(Debug)]
pub struct PlaySession {
    pub entity_id: i32,
    pub game_mode: GameMode,
    pub position: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    on_ground: bool,
    pending_teleport: Option<(i32, Instant)>,
    next_teleport: i32,
    pending_keep_alive: Option<(i64, Instant)>,
    next_keep_alive: Instant,
    chunks: ChunkStream,
    blocks: blocks::CreativeSession,
}

fn normalize_op(entry: &str) -> String {
    if let Ok(uuid) = uuid::Uuid::parse_str(entry) {
        uuid.to_string()
    } else {
        entry.to_ascii_lowercase()
    }
}

pub fn begin(ctx: &mut Context<'_>) -> io::Result<Outcome> {
    let profile = ctx
        .session
        .profile
        .clone()
        .ok_or_else(|| invalid("missing authenticated identity"))?;
    let entity_id = if let Some(id) = ctx.session.entity_id {
        id
    } else {
        let id = ctx
            .server
            .play
            .next_entity
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| io::Error::other("entity ID space exhausted"))?;
        ctx.session.entity_id = Some(id);
        id
    };
    let now = Instant::now();
    let view_distance = ctx
        .server
        .play
        .view_distance(ctx.session.client_information.as_ref());
    let mut chunks = ChunkStream::new(view_distance, now);
    if let Some(slot) = &ctx.session.reservation {
        slot.set_view([0, 0], view_distance);
    }
    let mut first_batch = chunks.pump(&ctx.server.play.terrain, now);
    let mut blocks = blocks::CreativeSession::new(ctx.server.play.blocks.clone());
    blocks.prepare_chunks(&mut first_batch);
    let game_mode = ctx.server.play.game_mode;
    let command_source = crate::command::CommandSource {
        permission: ctx.server.play.permission_for(profile.uuid, &profile.name),
        ..crate::command::CommandSource::player(profile.uuid)
    };
    ctx.session.play = Some(PlaySession {
        entity_id,
        game_mode,
        position: [world::SPAWN_X, world::SPAWN_Y, world::SPAWN_Z],
        yaw: 0.0,
        pitch: 0.0,
        on_ground: false,
        pending_teleport: Some((0, now)),
        next_teleport: 1,
        pending_keep_alive: None,
        next_keep_alive: now + KEEP_ALIVE_INTERVAL,
        chunks,
        blocks,
    });
    let mut replies = vec![
        JoinGame {
            entity_id,
            view_distance,
            game_mode,
        }
        .into(),
        PlayerInfo { profile, game_mode }.into(),
        PlayerAbilities(game_mode).into(),
        DefaultSpawnPosition.into(),
    ];
    replies.extend(ctx.server.play.initial_environment_packets());
    replies.extend([
        WorldBorder.into(),
        CenterChunk([0, 0]).into(),
        GameEvent::level_chunks_load_start().into(),
        DeclareCommands::new(ctx.server.play.commands(), &command_source).into(),
    ]);
    if game_mode == GameMode::Creative {
        replies.extend([
            Clientbound::from(InventoryContent(Box::new(ctx.session.inventory.clone()))),
            SelectedSlot(ctx.session.inventory.selected()).into(),
        ]);
    }
    replies.extend(first_batch);
    // The nearest chunks (including spawn) precede the initial teleport.
    // The rest of the view streams independently of teleport confirmation.
    replies.push(PlayerPosition::spawn(0).into());
    Ok(Outcome {
        replies,
        close: false,
        encryption: None,
    })
}

pub(crate) fn apply_control(
    session: &mut Session,
    service: &PlayService,
    control: crate::players::PlayerControl,
) -> io::Result<Outcome> {
    let slot = session.reservation.as_ref();
    let state = session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))?;
    match control {
        crate::players::PlayerControl::Teleport {
            position,
            yaw,
            pitch,
        } => {
            if !(-world::WORLD_LIMIT..=world::WORLD_LIMIT).contains(&position[0])
                || !(-world::WORLD_LIMIT..=world::WORLD_LIMIT).contains(&position[2])
                || !(f64::from(world::MIN_Y)..=f64::from(world::MIN_Y + world::HEIGHT))
                    .contains(&position[1])
            {
                return Ok(Outcome::empty());
            }
            let id = state.next_teleport;
            state.next_teleport = id
                .checked_add(1)
                .ok_or_else(|| invalid("teleport ID exhausted"))?;
            state.pending_teleport = Some((id, Instant::now()));
            state.position = position;
            if let Some(y) = yaw {
                state.yaw = y;
            }
            if let Some(p) = pitch {
                state.pitch = p;
            }
            state.on_ground = false;
            let old_view = state.chunks.view();
            state.chunks.follow(position[0], position[2]);
            let yaw = state.yaw;
            let pitch = state.pitch;
            if let Some(slot) = slot
                && old_view != state.chunks.view()
            {
                state.update_view(slot);
            }
            Ok(Outcome {
                replies: vec![PlayerPosition::at(position, yaw, pitch, id).into()],
                close: false,
                encryption: None,
            })
        }
        crate::players::PlayerControl::Give { item, count } => {
            session.inventory.give(item, count);
            Ok(Outcome {
                replies: vec![InventoryContent(Box::new(session.inventory.clone())).into()],
                close: false,
                encryption: None,
            })
        }
        crate::players::PlayerControl::SetGameMode(mode) => {
            state.game_mode = mode;
            let mut replies = vec![
                GameEvent::change_game_mode(mode).into(),
                PlayerAbilities(mode).into(),
            ];
            if mode == GameMode::Creative {
                replies.push(InventoryContent(Box::new(session.inventory.clone())).into());
                replies.push(SelectedSlot(session.inventory.selected()).into());
            }
            Ok(Outcome {
                replies,
                close: false,
                encryption: None,
            })
        }
        crate::players::PlayerControl::RefreshCommands => {
            let Some(profile) = &session.profile else {
                return Ok(Outcome::empty());
            };
            let source = crate::command::CommandSource {
                permission: service.permission_for(profile.uuid, &profile.name),
                ..crate::command::CommandSource::player(profile.uuid)
            };
            Ok(Outcome {
                replies: vec![DeclareCommands::new(service.commands(), &source).into()],
                close: false,
                encryption: None,
            })
        }
        crate::players::PlayerControl::Kick(reason) => Ok(closed(&reason)),
    }
}

fn play(session: &mut Session) -> io::Result<&mut PlaySession> {
    session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))
}

/// The network reports completed writes; response deadlines start when the
/// request actually leaves the output batch, not when that batch was built.
pub fn packet_sent(session: &mut Session, packet: &Clientbound, now: Instant) -> io::Result<()> {
    let Some(state) = session.play.as_mut() else {
        return Ok(());
    };
    state.chunks.packet_sent(packet, now);
    if let Clientbound::ChunkData(chunk) = packet
        && let Some(slot) = &session.reservation
    {
        slot.chunk_sent(chunk.0);
        state.blocks.chunk_sent(slot, chunk.0)?;
    }
    match packet {
        crate::protocol::Clientbound::PlayerPosition(packet) => {
            if state.pending_teleport.map(|(id, _)| id) == Some(packet.teleport_id) {
                state.pending_teleport = Some((packet.teleport_id, now));
                state.next_keep_alive = now + KEEP_ALIVE_INTERVAL;
            }
        }
        crate::protocol::Clientbound::KeepAlive(packet)
            if state.pending_keep_alive.map(|(id, _)| id) == Some(packet.id) =>
        {
            state.pending_keep_alive = Some((packet.id, now));
        }
        _ => {}
    }
    Ok(())
}

fn closed(reason: &str) -> Outcome {
    Outcome {
        replies: vec![
            PlayDisconnect {
                reason: reason.into(),
            }
            .into(),
        ],
        close: true,
        encryption: None,
    }
}

/// Called by the connection timer, not by a packet handler. An idle Play connection
/// is kept alive without imposing the old 30-second login lifetime on the world.
pub fn tick(session: &mut Session, now: Instant) -> io::Result<Outcome> {
    let state = play(session)?;
    if state
        .pending_teleport
        .is_some_and(|(_, sent)| now.duration_since(sent) >= TELEPORT_TIMEOUT)
    {
        return Ok(closed("ZCraft: teleport confirmation timed out."));
    }
    if state
        .pending_keep_alive
        .is_some_and(|(_, sent)| now.duration_since(sent) >= KEEP_ALIVE_TIMEOUT)
    {
        return Ok(closed("ZCraft: keep alive timed out."));
    }
    if state.chunks.feedback_timed_out(now) {
        return Ok(closed("ZCraft: chunk batch feedback timed out."));
    }
    if state.pending_keep_alive.is_none() && now >= state.next_keep_alive {
        let id = rand::random::<i64>();
        state.pending_keep_alive = Some((id, now));
        return Ok(Outcome {
            replies: vec![KeepAlive { id }.into()],
            close: false,
            encryption: None,
        });
    }
    Ok(Outcome::empty())
}

pub async fn handle_teleport(
    ctx: &mut Context<'_>,
    packet: TeleportConfirm,
) -> io::Result<Outcome> {
    let state = play(ctx.session)?;
    if state.pending_teleport.map(|(id, _)| id) != Some(packet.id) {
        return Err(invalid("unexpected teleport confirmation"));
    }
    state.pending_teleport = None;
    let entity_id = state.entity_id;
    let pose = state.pose();
    // ID 0 is reserved for initial world entry; subsequent teleports start at 1.
    if packet.id == 0 {
        let profile = ctx
            .session
            .profile
            .as_ref()
            .ok_or_else(|| invalid("missing authenticated identity"))?;
        tracing::info!(
            player = %profile.name,
            uuid = %profile.uuid,
            entity_id,
            "Player joined the server"
        );
        if ctx.session.configuration == super::configuration::ConfigurationPhase::Finished
            && let Some(slot) = &ctx.session.reservation
        {
            slot.set_chat_mode(
                ctx.session
                    .client_information
                    .as_ref()
                    .map_or(0, |info| info.chat_mode),
            );
            slot.show(Peer {
                uuid: profile.uuid,
                name: Arc::from(profile.name.as_str()),
                id: entity_id,
                pose,
                info: Clientbound::from(PlayerInfo {
                    profile: profile.clone(),
                    game_mode: ctx.server.play.game_mode,
                })
                .encode()?
                .into(),
            });
        }
    } else if let Some(slot) = &ctx.session.reservation {
        slot.update_pose(pose);
    }
    Ok(Outcome::empty())
}
pub async fn handle_keep_alive(
    ctx: &mut Context<'_>,
    packet: KeepAliveResponse,
) -> io::Result<Outcome> {
    let state = play(ctx.session)?;
    if state.pending_keep_alive.map(|(id, _)| id) != Some(packet.id) {
        return Err(invalid("unexpected keep alive response"));
    }
    state.pending_keep_alive = None;
    state.next_keep_alive = Instant::now() + KEEP_ALIVE_INTERVAL;
    Ok(Outcome::empty())
}
impl PlaySession {
    fn pose(&self) -> EntityPose {
        EntityPose::new(self.position, self.yaw, self.pitch, self.on_ground)
    }

    fn update_view(&mut self, slot: &crate::players::Reservation) {
        let (center, radius) = self.chunks.view();
        slot.set_view(center, radius);
        slot.retire_blocks(&self.chunks.retire_outside());
    }
}

fn movement(
    ctx: &mut Context<'_>,
    position: Option<[f64; 3]>,
    rotation: Option<[f32; 2]>,
    on_ground: bool,
) -> io::Result<Outcome> {
    let slot = ctx.session.reservation.as_ref();
    let state = ctx
        .session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))?;
    let old_view = state.chunks.view();
    if state.pending_teleport.is_some() {
        return Ok(Outcome::empty());
    }
    if let Some([x, y, z]) = position {
        if !(-world::WORLD_LIMIT..=world::WORLD_LIMIT).contains(&x)
            || !(-world::WORLD_LIMIT..=world::WORLD_LIMIT).contains(&z)
            || !(f64::from(world::MIN_Y)..=f64::from(world::MIN_Y + world::HEIGHT)).contains(&y)
        {
            let id = state.next_teleport;
            state.next_teleport = id
                .checked_add(1)
                .ok_or_else(|| invalid("teleport ID exhausted"))?;
            state.pending_teleport = Some((id, Instant::now()));
            state.position = [world::SPAWN_X, world::SPAWN_Y, world::SPAWN_Z];
            state.chunks.follow(world::SPAWN_X, world::SPAWN_Z);
            state.yaw = 0.0;
            state.pitch = 0.0;
            state.on_ground = false;
            if let Some(slot) = slot
                && old_view != state.chunks.view()
            {
                state.update_view(slot);
            }
            return Ok(Outcome {
                replies: vec![PlayerPosition::spawn(id).into()],
                close: false,
                encryption: None,
            });
        }
        state.position = [x, y, z];
        state.chunks.follow(x, z);
    }
    if let Some([yaw, pitch]) = rotation {
        state.yaw = yaw;
        state.pitch = pitch;
    }
    state.on_ground = on_ground;
    let pose = state.pose();
    if let Some(slot) = slot {
        if old_view != state.chunks.view() {
            state.update_view(slot);
        }
        slot.update_pose(pose);
    }
    Ok(Outcome::empty())
}
pub async fn handle_position(ctx: &mut Context<'_>, p: Position) -> io::Result<Outcome> {
    movement(ctx, Some([p.x, p.y, p.z]), None, p.on_ground)
}
pub async fn handle_position_look(ctx: &mut Context<'_>, p: PositionLook) -> io::Result<Outcome> {
    movement(
        ctx,
        Some([p.x, p.y, p.z]),
        Some([p.yaw, p.pitch]),
        p.on_ground,
    )
}
pub async fn handle_look(ctx: &mut Context<'_>, p: Look) -> io::Result<Outcome> {
    movement(ctx, None, Some([p.yaw, p.pitch]), p.on_ground)
}
pub async fn handle_flying(ctx: &mut Context<'_>, p: Flying) -> io::Result<Outcome> {
    movement(ctx, None, None, p.on_ground)
}
pub async fn handle_batch(ctx: &mut Context<'_>, p: ChunkBatchReceived) -> io::Result<Outcome> {
    play(ctx.session)?
        .chunks
        .feedback(p.chunks_per_tick, Instant::now())?;
    Ok(Outcome::empty())
}

pub fn next_chunk_update(session: &Session) -> Option<Instant> {
    session.play.as_ref()?.chunks.next_update()
}

/// Paces terrain independently from keep-alive and inbound movement packets.
pub fn stream_chunks(
    session: &mut Session,
    service: &PlayService,
    now: Instant,
) -> io::Result<Outcome> {
    let state = play(session)?;
    let mut packets = state.chunks.pump(&service.terrain, now);
    state.blocks.prepare_chunks(&mut packets);
    let mut replies = Vec::new();
    if let Some(slot) = &session.reservation {
        let unloaded: std::collections::HashSet<_> = packets
            .iter()
            .filter_map(|packet| {
                if let Clientbound::ForgetChunk(chunk) = packet {
                    Some(chunk.0)
                } else {
                    None
                }
            })
            .collect();
        if !unloaded.is_empty() {
            replies = slot.before_unload(&unloaded);
        }
    }
    replies.extend(packets);
    Ok(Outcome {
        replies,
        close: false,
        encryption: None,
    })
}
pub async fn handle_information(
    ctx: &mut Context<'_>,
    packet: PlayClientInformation,
) -> io::Result<Outcome> {
    let distance = ctx.server.play.view_distance(Some(&packet.0));
    let state = ctx
        .session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))?;
    if state.chunks.view().1 != distance {
        state.chunks.set_radius(distance);
        if let Some(slot) = &ctx.session.reservation {
            state.update_view(slot);
        }
    }
    if let Some(slot) = &ctx.session.reservation {
        slot.set_chat_mode(packet.0.chat_mode);
    }
    ctx.session.client_information = Some(packet.0);
    Ok(Outcome::empty())
}
pub async fn handle_action(ctx: &mut Context<'_>, packet: EntityAction) -> io::Result<Outcome> {
    if play(ctx.session)?.entity_id != packet.entity_id {
        return Err(invalid("entity action refers to another player"));
    }
    Ok(Outcome::empty())
}
/// These packets have no effect in this read-only prototype. In particular a
/// chat-session certificate is never treated as an identity/authentication proof.
pub async fn ignore<T>(_ctx: &mut Context<'_>, _packet: T) -> io::Result<Outcome> {
    Ok(Outcome::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Clientbound;
    #[test]
    fn client_view_distance_is_capped_by_the_service_limit() {
        use crate::protocol::{Decode, Reader, packets::common::ClientInformation};

        let service = PlayService::with_max_view_distance(6).unwrap();
        assert_eq!(service.view_distance(None), 6);
        for (requested, expected) in [(2, 2), (4, 4), (6, 6), (127, 6)] {
            let bytes = [2, b'e', b'n', requested, 0, 1, 127, 1, 0, 1];
            let information = ClientInformation::decode(&mut Reader::new(&bytes)).unwrap();
            assert_eq!(service.view_distance(Some(&information)), expected);
        }
    }

    #[test]
    fn keep_alive_and_teleport_deadlines_are_independent() {
        let now = Instant::now();
        let mut session = Session {
            play: Some(PlaySession {
                entity_id: 1,
                game_mode: GameMode::Spectator,
                position: [0.0; 3],
                yaw: 0.0,
                pitch: 0.0,
                on_ground: false,
                pending_teleport: None,
                next_teleport: 1,
                pending_keep_alive: None,
                next_keep_alive: now,
                chunks: ChunkStream::new(2, now),
                blocks: blocks::CreativeSession::new(Arc::default()),
            }),
            ..Session::default()
        };
        let outcome = tick(&mut session, now).unwrap();
        assert!(matches!(
            outcome.replies.as_slice(),
            [Clientbound::KeepAlive(_)]
        ));
        assert!(
            !tick(&mut session, now + Duration::from_secs(29))
                .unwrap()
                .close
        );
        assert!(
            tick(&mut session, now + Duration::from_secs(30))
                .unwrap()
                .close
        );
        let state = session.play.as_mut().unwrap();
        state.pending_keep_alive = None;
        state.pending_teleport = Some((1, now));
        assert!(
            tick(&mut session, now + Duration::from_secs(15))
                .unwrap()
                .close
        );
    }
}
