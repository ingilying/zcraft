//! Play-phase protocol IO adapter: manages connection-scoped protocol concerns
//! (keep-alive, teleport ID tracking, chunk streaming) and forwards all gameplay
//! actions to the decoupled `bevy_ecs` `GameEngine`.
pub(crate) mod blocks;
pub(crate) mod chat;
mod chunks;
mod control;
mod service;
pub(crate) mod sync;

use self::chunks::ChunkStream;
pub(crate) use self::control::apply_control;
pub use self::service::PlayService;
use super::{Context, Outcome, Session};
use crate::{
    config::GameMode,
    game::GameInput,
    players::Peer,
    protocol::{
        Clientbound, invalid,
        packets::{
            play::*,
            play_blocks::{InventoryContent, SelectedSlot},
            play_chat::DeclareCommands,
            play_common::*,
        },
    },
    world,
};
use std::{io, sync::Arc, time::Duration};
use tokio::time::Instant;
use uuid::Uuid;

const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const TELEPORT_TIMEOUT: Duration = Duration::from_secs(15);
const GAMEPLAY_RATE_BURST: f64 = 128.0;
const GAMEPLAY_RATE_PER_SEC: f64 = 100.0;

/// Per-connection token bucket preventing a single noisy/malicious client from
/// saturating the shared `GAME_INPUT_QUEUE_CAPACITY` channel and impacting other players.
#[derive(Debug)]
struct GameplayRate {
    tokens: f64,
    last: Instant,
}

impl GameplayRate {
    fn new(now: Instant) -> Self {
        Self {
            tokens: GAMEPLAY_RATE_BURST,
            last: now,
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * GAMEPLAY_RATE_PER_SEC).min(GAMEPLAY_RATE_BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Protocol/transport-only Play session state. Authoritative gameplay state
/// (player physics, vitals, inventory, containers, mining) lives in `crate::game`.
#[derive(Debug)]
pub struct PlaySession {
    pub uuid: Uuid,
    pub entity_id: i32,
    pending_teleport: Option<(i32, Instant)>,
    next_teleport: i32,
    pending_keep_alive: Option<(i64, Instant)>,
    next_keep_alive: Instant,
    chunks: ChunkStream,
    blocks: blocks::CreativeSession,
    gameplay_rate: GameplayRate,
}

pub fn begin(ctx: &mut Context<'_>) -> io::Result<Outcome> {
    let profile = ctx
        .session
        .profile
        .clone()
        .ok_or_else(|| invalid("missing authenticated identity"))?;
    // Fail-closed on corrupt/unreadable playerdata so we never overwrite a
    // player's existing save with default spawn/inventory state on disconnect.
    let saved = ctx.server.play.load_player(profile.uuid).map_err(|error| {
        tracing::error!(uuid = %profile.uuid, %error, "failed to load playerdata; rejecting login to prevent overwrite");
        error
    })?;
    let now = Instant::now();
    let world = ctx.server.play.world().clone();
    let view_distance = ctx
        .server
        .play
        .view_distance(ctx.session.client_information.as_ref());
    let position = saved
        .as_ref()
        .map_or_else(|| world.spawn_position(), |s| s.position);
    let spawn_chunk = world::chunk_at(position[0], position[2]);
    let mut chunks = ChunkStream::at(spawn_chunk, view_distance, now);
    let mut first_batch = chunks.pump(world.terrain(), now);
    let mut blocks = blocks::CreativeSession::new(world);
    blocks.prepare_chunks(&mut first_batch);
    let (join_input, reply_ctx) =
        prepare_begin(ctx, profile, saved, now, view_distance, chunks, blocks)?;
    ctx.server
        .play
        .submit_input_sync(join_input, ctx.server.players())?;
    let drops = ctx.server.play.existing_drop_packets();
    Ok(build_begin_outcome(ctx, reply_ctx, first_batch, drops))
}

/// Async Play entry used by the network runtime: offloads playerdata disk reads
/// and initial chunk generation/lighting/encoding onto Tokio's blocking pool, and
/// uses non-blocking `oneshot` barriers/queries against the dedicated game loop.
pub(crate) async fn begin_async(ctx: &mut Context<'_>) -> io::Result<Outcome> {
    let profile = ctx
        .session
        .profile
        .clone()
        .ok_or_else(|| invalid("missing authenticated identity"))?;
    // Ensure any recent PlayerLeave persistence for a rapid reconnect is flushed first.
    ctx.server.play.flush_player_saves_async().await;
    let world = ctx.server.play.world().clone();
    let uuid = profile.uuid;
    let load_world = world.clone();
    let saved = tokio::task::spawn_blocking(move || load_world.load_player(uuid))
        .await
        .map_err(io::Error::other)?
        .map_err(|error| {
            tracing::error!(%uuid, %error, "failed to load playerdata; rejecting login to prevent overwrite");
            error
        })?;
    let now = Instant::now();
    let view_distance = ctx
        .server
        .play
        .view_distance(ctx.session.client_information.as_ref());
    let position = saved
        .as_ref()
        .map_or_else(|| world.spawn_position(), |s| s.position);
    let spawn_chunk = world::chunk_at(position[0], position[2]);
    let mut chunks = ChunkStream::at(spawn_chunk, view_distance, now);
    let mut first_batch = chunks.pump(world.terrain(), now);
    let mut blocks = blocks::CreativeSession::new(world);
    blocks.prepare_chunks_async(&mut first_batch).await?;
    let (join_input, reply_ctx) =
        prepare_begin(ctx, profile, saved, now, view_distance, chunks, blocks)?;
    ctx.server
        .play
        .submit_input_async(join_input, ctx.server.players())
        .await?;
    let drops = ctx.server.play.existing_drop_packets_async().await;
    Ok(build_begin_outcome(ctx, reply_ctx, first_batch, drops))
}

struct BeginReplyContext {
    profile: crate::identity::Profile,
    entity_id: i32,
    view_distance: i32,
    position: [f64; 3],
    yaw: f32,
    pitch: f32,
    game_mode: GameMode,
    health: f32,
    food: i32,
    saturation: f32,
    spawn_chunk: [i32; 2],
    command_source: crate::command::CommandSource,
    inventory_snapshot: crate::game::Inventory,
}

#[allow(clippy::too_many_arguments)]
fn prepare_begin(
    ctx: &mut Context<'_>,
    profile: crate::identity::Profile,
    saved: Option<world::SavedPlayer>,
    now: Instant,
    view_distance: i32,
    chunks: ChunkStream,
    blocks: blocks::CreativeSession,
) -> io::Result<(GameInput, BeginReplyContext)> {
    let entity_id = if let Some(id) = ctx.session.entity_id {
        id
    } else {
        let id = ctx.server.play.allocate_entity_id()?;
        ctx.session.entity_id = Some(id);
        id
    };
    let world = ctx.server.play.world();
    let (position, yaw, pitch, on_ground, game_mode, health, food, saturation, respawn_point) =
        if let Some(saved) = saved {
            ctx.session.inventory = saved.inventory;
            (
                saved.position,
                saved.yaw,
                saved.pitch,
                saved.on_ground,
                saved.game_mode,
                saved.health,
                saved.food,
                saved.saturation,
                saved.respawn_point,
            )
        } else {
            (
                world.spawn_position(),
                0.0,
                0.0,
                false,
                ctx.server.play.game_mode,
                20.0,
                20,
                5.0,
                None,
            )
        };
    let spawn_chunk = world::chunk_at(position[0], position[2]);
    if let Some(slot) = &ctx.session.reservation {
        slot.set_view(spawn_chunk, view_distance);
    }
    let command_source = crate::command::CommandSource {
        permission: ctx.server.play.permission_for(profile.uuid, &profile.name),
        ..crate::command::CommandSource::player(profile.uuid)
    };
    let mut player =
        crate::game::Player::at_spawn(entity_id, profile.uuid, profile.name.clone(), game_mode);
    player.position = position;
    player.yaw = yaw;
    player.pitch = pitch;
    player.on_ground = on_ground;
    player.health = health;
    player.food = food;
    player.saturation = saturation;
    player.respawn_point = respawn_point;
    player.dead = health <= 0.0;

    let inventory_snapshot = ctx.session.inventory.clone();
    let join_input = GameInput::PlayerJoin {
        player: Box::new(player),
        inventory: Box::new(inventory_snapshot.clone()),
    };

    ctx.session.play = Some(PlaySession {
        uuid: profile.uuid,
        entity_id,
        pending_teleport: Some((0, now)),
        next_teleport: 1,
        pending_keep_alive: None,
        next_keep_alive: now + KEEP_ALIVE_INTERVAL,
        chunks,
        blocks,
        gameplay_rate: GameplayRate::new(now),
    });
    tracing::info!(
        player = %profile.name,
        uuid = %profile.uuid,
        game_mode = ?game_mode,
        "joined the game"
    );
    Ok((
        join_input,
        BeginReplyContext {
            profile,
            entity_id,
            view_distance,
            position,
            yaw,
            pitch,
            game_mode,
            health,
            food,
            saturation,
            spawn_chunk,
            command_source,
            inventory_snapshot,
        },
    ))
}

fn build_begin_outcome(
    ctx: &Context<'_>,
    info: BeginReplyContext,
    first_batch: Vec<Clientbound>,
    existing_drops: Vec<Clientbound>,
) -> Outcome {
    let world = ctx.server.play.world();
    let is_flat = world.is_flat();
    let max_players = ctx.server.players().max_players() as i32;
    let mut replies = vec![
        JoinGame {
            entity_id: info.entity_id,
            max_players,
            view_distance: info.view_distance,
            simulation_distance: info.view_distance,
            game_mode: info.game_mode,
            is_flat,
        }
        .into(),
        ChangeDifficulty::normal().into(),
        PlayerInfo {
            profile: info.profile,
            game_mode: info.game_mode,
        }
        .into(),
        PlayerAbilities(info.game_mode).into(),
        DefaultSpawnPosition::from_spawn(world.spawn_position()).into(),
    ];
    replies.extend(ctx.server.play.initial_environment_packets());
    replies.extend([
        WorldBorder.into(),
        CenterChunk(info.spawn_chunk).into(),
        GameEvent::level_chunks_load_start().into(),
        DeclareCommands::new(ctx.server.play.commands(), &info.command_source).into(),
    ]);
    if info.game_mode == GameMode::Survival {
        replies.push(UpdateHealth::new(info.health, info.food, info.saturation).into());
    }
    if info.game_mode != GameMode::Spectator {
        replies.extend([
            Clientbound::from(InventoryContent::player(&info.inventory_snapshot)),
            SelectedSlot(info.inventory_snapshot.selected()).into(),
        ]);
    }
    replies.extend(first_batch);
    replies.extend(existing_drops);
    // The nearest chunks (including spawn) precede the initial teleport.
    // The rest of the view streams independently of teleport confirmation.
    replies.push(PlayerPosition::at(info.position, info.yaw, info.pitch, 0).into());
    Outcome {
        replies,
        close: false,
        encryption: None,
    }
}

/// Persist and despawn a player's ECS entity when leaving Play.
pub(crate) fn save_session(
    session: &mut Session,
    play: &PlayService,
    players: &crate::players::Players,
) {
    let Some(play_session) = session.play.take() else {
        return;
    };
    let _ = play.submit_input_sync(
        GameInput::PlayerLeave {
            uuid: play_session.uuid,
            persist: true,
        },
        players,
    );
}

/// Async counterpart of `save_session` for normal connection teardown: uses the
/// non-blocking `oneshot` FIFO barrier while persistence runs on the dedicated
/// player-save worker. Idempotent with `Drop::drop`.
pub(crate) async fn save_session_async(
    session: &mut Session,
    server: &Arc<crate::context::ServerContext>,
) {
    let Some(play_session) = session.play.take() else {
        return;
    };
    let _ = server
        .play
        .submit_input_async(
            GameInput::PlayerLeave {
                uuid: play_session.uuid,
                persist: true,
            },
            server.players(),
        )
        .await;
}

fn play(session: &mut Session) -> io::Result<&mut PlaySession> {
    session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))
}

pub(super) fn check_gameplay_rate(session: &mut Session) -> io::Result<Uuid> {
    let state = play(session)?;
    if !state.gameplay_rate.allow(Instant::now()) {
        return Err(invalid("gameplay packet rate limit exceeded"));
    }
    Ok(state.uuid)
}

/// The network reports completed writes; response deadlines start when the
/// request actually leaves the output batch, not when that batch was built.
pub fn packet_sent(
    session: &mut Session,
    play_service: &PlayService,
    players: &crate::players::Players,
    packet: &Clientbound,
    now: Instant,
) -> io::Result<()> {
    let Some(state) = session.play.as_mut() else {
        return Ok(());
    };
    state.chunks.packet_sent(packet, now);
    if let Clientbound::ChunkData(chunk) = packet {
        if let Some(slot) = &session.reservation {
            slot.chunk_sent(chunk.0);
            state.blocks.chunk_sent(slot, chunk.0)?;
        }
        let _ = play_service.dispatch_input(
            GameInput::PlayerChunkTracked {
                uuid: state.uuid,
                chunk: chunk.0,
            },
            players,
        );
    }
    match packet {
        Clientbound::PlayerPosition(packet) => {
            if state.pending_teleport.map(|(id, _)| id) == Some(packet.teleport_id) {
                state.pending_teleport = Some((packet.teleport_id, now));
                state.next_keep_alive = now + KEEP_ALIVE_INTERVAL;
            }
        }
        Clientbound::KeepAlive(packet)
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

/// Connection protocol heartbeat & timeout check (KeepAlive, teleport deadline,
/// chunk-batch feedback deadline). All gameplay ticking runs in the ECS Game Loop.
pub fn tick(session: &mut Session, now: Instant) -> io::Result<Outcome> {
    let state = session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))?;
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
    let mut replies = Vec::new();
    if state.pending_keep_alive.is_none() && now >= state.next_keep_alive {
        let id = rand::random::<i64>();
        state.pending_keep_alive = Some((id, now));
        replies.push(KeepAlive { id }.into());
    }
    Ok(Outcome {
        replies,
        close: false,
        encryption: None,
    })
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
    let uuid = state.uuid;
    let entity_id = state.entity_id;

    // Notify ECS that teleport is confirmed so movement & block interactions unlock.
    ctx.server
        .play
        .submit_input_async(
            GameInput::PlayerTeleportConfirmed { uuid },
            ctx.server.players(),
        )
        .await?;

    // ID 0 is reserved for initial world entry; subsequent teleports start at 1.
    if packet.id == 0 {
        let profile = ctx
            .session
            .profile
            .as_ref()
            .ok_or_else(|| invalid("missing authenticated identity"))?;
        let snap = ctx
            .server
            .play
            .player_snapshot_async(uuid)
            .await
            .ok_or_else(|| invalid("player entity missing"))?;
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
                game_mode: snap.game_mode,
                pose: snap.pose,
                info: Clientbound::from(PlayerInfo {
                    profile: profile.clone(),
                    game_mode: snap.game_mode,
                })
                .encode()?
                .into(),
            });
        }
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
    fn update_view(
        &mut self,
        slot: &crate::players::Reservation,
        service: &PlayService,
        players: &crate::players::Players,
    ) -> io::Result<()> {
        let (center, radius) = self.chunks.view();
        slot.set_view(center, radius);
        let retired = self.chunks.retire_outside();
        if !retired.is_empty() {
            slot.retire_blocks(&retired);
            service.dispatch_input(
                GameInput::PlayerChunksUntracked {
                    uuid: self.uuid,
                    chunks: retired,
                },
                players,
            )?;
        }
        Ok(())
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
    if !state.gameplay_rate.allow(Instant::now()) {
        return Err(invalid("gameplay packet rate limit exceeded"));
    }
    if state.pending_teleport.is_some() {
        return Ok(Outcome::empty());
    }
    let uuid = state.uuid;
    if let Some([x, y, z]) = position
        && crate::game::Player::position_in_bounds([x, y, z])
    {
        let old_view = state.chunks.view();
        state.chunks.follow(x, z);
        if let Some(slot) = slot
            && old_view != state.chunks.view()
        {
            state.update_view(slot, &ctx.server.play, ctx.server.players())?;
        }
    }
    ctx.server.play.dispatch_input(
        GameInput::PlayerMove {
            uuid,
            position,
            rotation,
            on_ground,
        },
        ctx.server.players(),
    )?;
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
    let mut packets = state.chunks.pump(service.world().terrain(), now);
    state.blocks.prepare_chunks(&mut packets);
    finish_stream_chunks(session, packets)
}

/// Async chunk streaming used by the network loop: offloads chunk loading,
/// generation, lighting, encoding, and dirty-cache writeback onto Tokio's
/// blocking pool.
pub(crate) async fn stream_chunks_async(
    session: &mut Session,
    service: &PlayService,
    now: Instant,
) -> io::Result<Outcome> {
    let state = play(session)?;
    let mut packets = state.chunks.pump(service.world().terrain(), now);
    state.blocks.prepare_chunks_async(&mut packets).await?;
    finish_stream_chunks(session, packets)
}

fn finish_stream_chunks(session: &Session, packets: Vec<Clientbound>) -> io::Result<Outcome> {
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
            state.update_view(slot, &ctx.server.play, ctx.server.players())?;
        }
    }
    if let Some(slot) = &ctx.session.reservation {
        slot.set_chat_mode(packet.0.chat_mode);
    }
    ctx.session.client_information = Some(packet.0);
    Ok(Outcome::empty())
}

pub async fn handle_action(ctx: &mut Context<'_>, packet: EntityAction) -> io::Result<Outcome> {
    let state = play(ctx.session)?;
    if state.entity_id != packet.entity_id {
        return Err(invalid("entity action refers to another player"));
    }
    if !state.gameplay_rate.allow(Instant::now()) {
        return Err(invalid("gameplay packet rate limit exceeded"));
    }
    let uuid = state.uuid;
    ctx.server.play.dispatch_input(
        GameInput::PlayerEntityAction {
            uuid,
            action: packet.action,
        },
        ctx.server.players(),
    )?;
    Ok(Outcome::empty())
}

pub async fn handle_client_command(
    ctx: &mut Context<'_>,
    packet: ClientCommand,
) -> io::Result<Outcome> {
    if packet.action != 0 {
        return Ok(Outcome::empty());
    }
    let uuid = check_gameplay_rate(ctx.session)?;
    // Preload the player's bed respawn chunk on Tokio's blocking pool so the
    // dedicated game-loop thread never blocks on disk I/O during `handle_respawn`.
    if let Some(bed_pos) = ctx
        .server
        .play
        .player_snapshot_async(uuid)
        .await
        .and_then(|snap| snap.respawn_point)
        && bed_pos.valid()
    {
        let world = ctx.server.play.world().clone();
        let chunk = bed_pos.chunk();
        let _ = tokio::task::spawn_blocking(move || world.ensure_chunk_loaded(chunk)).await;
    }
    ctx.server
        .play
        .dispatch_input(GameInput::PlayerRespawn { uuid }, ctx.server.players())?;
    Ok(Outcome::empty())
}

pub async fn handle_arm_animation(
    ctx: &mut Context<'_>,
    packet: ArmAnimation,
) -> io::Result<Outcome> {
    let uuid = check_gameplay_rate(ctx.session)?;
    ctx.server.play.dispatch_input(
        GameInput::PlayerArmAnimation {
            uuid,
            hand: packet.hand,
        },
        ctx.server.players(),
    )?;
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
                uuid: Uuid::nil(),
                entity_id: 1,
                pending_teleport: None,
                next_teleport: 1,
                pending_keep_alive: None,
                next_keep_alive: now,
                chunks: ChunkStream::new(2, now),
                blocks: blocks::CreativeSession::new(Arc::default()),
                gameplay_rate: GameplayRate::new(now),
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

    #[test]
    fn gameplay_rate_limiter_caps_burst_and_refills_over_time() {
        let now = Instant::now();
        let mut rate = GameplayRate::new(now);
        for _ in 0..(GAMEPLAY_RATE_BURST as usize) {
            assert!(rate.allow(now));
        }
        assert!(!rate.allow(now));
        // After 50ms at 100 packets/sec, 5 tokens refill.
        let later = now + Duration::from_millis(50);
        for _ in 0..5 {
            assert!(rate.allow(later));
        }
        assert!(!rate.allow(later));
    }
}
