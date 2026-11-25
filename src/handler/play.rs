//! Minimal flat world with spectator and server-authoritative creative modes.
pub(crate) mod blocks;
pub(crate) mod chat;
mod chunks;
mod control;
mod service;

use self::chunks::ChunkStream;
pub(crate) use self::control::apply_control;
pub use self::service::PlayService;
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
        },
    },
    world,
};
use std::{io, sync::Arc, time::Duration};
use tokio::time::Instant;

const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const TELEPORT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub struct PlaySession {
    pub player: crate::game::Player,
    pending_teleport: Option<(i32, Instant)>,
    next_teleport: i32,
    pending_keep_alive: Option<(i64, Instant)>,
    next_keep_alive: Instant,
    chunks: ChunkStream,
    blocks: blocks::CreativeSession,
    open_container: Option<blocks::OpenContainer>,
    next_window_id: u8,
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
        let id = ctx.server.play.allocate_entity_id()?;
        ctx.session.entity_id = Some(id);
        id
    };
    let now = Instant::now();
    let view_distance = ctx
        .server
        .play
        .view_distance(ctx.session.client_information.as_ref());
    let world = ctx.server.play.world();
    // Fail-closed on corrupt/unreadable playerdata so we never overwrite a
    // player's existing save with default spawn/inventory state on disconnect.
    let saved = ctx.server.play.load_player(profile.uuid).map_err(|error| {
        tracing::error!(uuid = %profile.uuid, %error, "failed to load playerdata; rejecting login to prevent overwrite");
        error
    })?;
    let (position, yaw, pitch, on_ground, game_mode, health, food, saturation) =
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
            )
        };
    let spawn_chunk = world::chunk_at(position[0], position[2]);
    let mut chunks = ChunkStream::at(spawn_chunk, view_distance, now);
    if let Some(slot) = &ctx.session.reservation {
        slot.set_view(spawn_chunk, view_distance);
    }
    let mut first_batch = chunks.pump(world.terrain(), now);
    let mut blocks = blocks::CreativeSession::new(world.clone());
    blocks.prepare_chunks(&mut first_batch);
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
    player.dead = health <= 0.0;
    ctx.session.play = Some(PlaySession {
        player,
        pending_teleport: Some((0, now)),
        next_teleport: 1,
        pending_keep_alive: None,
        next_keep_alive: now + KEEP_ALIVE_INTERVAL,
        chunks,
        blocks,
        open_container: None,
        next_window_id: 1,
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
        CenterChunk(spawn_chunk).into(),
        GameEvent::level_chunks_load_start().into(),
        DeclareCommands::new(ctx.server.play.commands(), &command_source).into(),
    ]);
    if game_mode == GameMode::Survival {
        replies.push(UpdateHealth::new(health, food, saturation).into());
    }
    if game_mode != GameMode::Spectator {
        replies.extend([
            Clientbound::from(InventoryContent::player(&ctx.session.inventory)),
            SelectedSlot(ctx.session.inventory.selected()).into(),
        ]);
    }
    replies.extend(first_batch);
    // The nearest chunks (including spawn) precede the initial teleport.
    // The rest of the view streams independently of teleport confirmation.
    replies.push(PlayerPosition::at(position, yaw, pitch, 0).into());
    Ok(Outcome {
        replies,
        close: false,
        encryption: None,
    })
}

/// Best-effort persistence of a player's state and inventory when leaving Play.
pub(crate) fn save_session(session: &Session, play: &PlayService) {
    let Some(play_session) = &session.play else {
        return;
    };
    if let Err(error) = play.save_player(&play_session.player, &session.inventory) {
        tracing::warn!(
            uuid = %play_session.player.uuid,
            %error,
            "failed to save playerdata"
        );
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
    let entity_id = state.player.entity_id;
    let game_mode = state.player.game_mode;
    let pose = state.player.pose();
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
                game_mode,
                pose,
                info: Clientbound::from(PlayerInfo {
                    profile: profile.clone(),
                    game_mode,
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
    let old_y = state.player.position[1];
    if let Some([x, y, z]) = position {
        if !crate::game::Player::position_in_bounds([x, y, z]) {
            if state.player.is_survival() && y < f64::from(world::MIN_Y) - 4.0 {
                // Void damage below the world floor.
                let mut replies = Vec::new();
                if state.player.apply_damage(4.0) {
                    let hurt = Clientbound::from(HurtAnimation::new(state.player.entity_id, 0.0));
                    if let Some(slot) = slot {
                        let _ = slot.broadcast_to_viewers(&hurt);
                    }
                    replies.push(hurt);
                    replies.push(
                        UpdateHealth::new(
                            state.player.health,
                            state.player.food,
                            state.player.saturation,
                        )
                        .into(),
                    );
                }
                return Ok(Outcome {
                    replies,
                    close: false,
                    encryption: None,
                });
            }
            let id = state.next_teleport;
            state.next_teleport = id
                .checked_add(1)
                .ok_or_else(|| invalid("teleport ID exhausted"))?;
            state.pending_teleport = Some((id, Instant::now()));
            state.player.reset_to_spawn();
            state.chunks.follow(world::SPAWN_X, world::SPAWN_Z);
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
        state.player.position = [x, y, z];
        state.chunks.follow(x, z);
    }
    if let Some([yaw, pitch]) = rotation {
        state.player.yaw = yaw;
        state.player.pitch = pitch;
    }
    state.player.on_ground = on_ground;
    let mut replies = Vec::new();
    if state.player.is_survival() && !state.player.dead {
        let [px, py, pz] = state.player.position;
        let feet_pos = world::BlockPos([px.floor() as i32, py.floor() as i32, pz.floor() as i32]);
        let feet_state = ctx.server.play.world().block_at(feet_pos);
        let in_liquid = crate::game::block_by_state(feet_state).is_some_and(|d| d.liquid);
        if in_liquid {
            state.player.fall_distance = 0.0;
        } else if on_ground {
            let fall = std::mem::take(&mut state.player.fall_distance);
            if fall > 3.0 {
                let damage = (fall - 3.0).floor() as f32;
                if state.player.apply_damage(damage) {
                    let hurt = Clientbound::from(HurtAnimation::new(state.player.entity_id, 0.0));
                    if let Some(slot) = slot {
                        let _ = slot.broadcast_to_viewers(&hurt);
                    }
                    replies.push(hurt);
                    replies.push(
                        UpdateHealth::new(
                            state.player.health,
                            state.player.food,
                            state.player.saturation,
                        )
                        .into(),
                    );
                }
            }
        } else if py < old_y {
            state.player.fall_distance += old_y - py;
        }
    }
    let pose = state.player.pose();
    if let Some(slot) = slot {
        if old_view != state.chunks.view() {
            state.update_view(slot);
        }
        slot.update_pose(pose);
    }
    Ok(Outcome {
        replies,
        close: false,
        encryption: None,
    })
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
    let state = play(ctx.session)?;
    if state.player.entity_id != packet.entity_id {
        return Err(invalid("entity action refers to another player"));
    }
    let mut changed = false;
    match packet.action {
        0 if !state.player.sneaking => {
            state.player.sneaking = true;
            changed = true;
        }
        1 if state.player.sneaking => {
            state.player.sneaking = false;
            changed = true;
        }
        3 if !state.player.sprinting => {
            state.player.sprinting = true;
            changed = true;
        }
        4 if state.player.sprinting => {
            state.player.sprinting = false;
            changed = true;
        }
        _ => {}
    }
    let (entity_id, sneaking, sprinting) = (
        state.player.entity_id,
        state.player.sneaking,
        state.player.sprinting,
    );
    if changed && let Some(slot) = &ctx.session.reservation {
        let packet = Clientbound::from(
            crate::protocol::packets::play_entities::EntityMetadata::Player {
                id: entity_id,
                sneaking,
                sprinting,
            },
        );
        slot.broadcast_to_viewers(&packet)?;
    }
    Ok(Outcome::empty())
}

pub async fn handle_client_command(
    ctx: &mut Context<'_>,
    packet: ClientCommand,
) -> io::Result<Outcome> {
    if packet.action != 0 {
        return Ok(Outcome::empty());
    }
    let spawn = ctx.server.play.world().spawn_position();
    let is_flat = ctx.server.play.world().flat_config().is_some();
    let state = play(ctx.session)?;
    if !state.player.dead {
        return Ok(Outcome::empty());
    }
    let tp_id = state.next_teleport;
    state.next_teleport = tp_id
        .checked_add(1)
        .ok_or_else(|| invalid("teleport ID exhausted"))?;
    state.pending_teleport = Some((tp_id, Instant::now()));
    state.player.respawn_at(spawn);
    state.chunks.follow(spawn[0], spawn[2]);
    let game_mode = state.player.game_mode;
    let health = state.player.health;
    let food = state.player.food;
    let saturation = state.player.saturation;
    let pose = state.player.pose();
    let view = state.chunks.view();
    if let Some(slot) = &ctx.session.reservation {
        slot.set_view(view.0, view.1);
        slot.update_pose(pose);
    }
    let mut replies = vec![
        Respawn { game_mode, is_flat }.into(),
        PlayerAbilities(game_mode).into(),
        UpdateHealth::new(health, food, saturation).into(),
        PlayerPosition::at(spawn, 0.0, 0.0, tp_id).into(),
    ];
    if game_mode != GameMode::Spectator {
        replies.push(InventoryContent::player(&ctx.session.inventory).into());
        replies.push(SelectedSlot(ctx.session.inventory.selected()).into());
    }
    Ok(Outcome {
        replies,
        close: false,
        encryption: None,
    })
}

pub async fn handle_arm_animation(
    ctx: &mut Context<'_>,
    packet: ArmAnimation,
) -> io::Result<Outcome> {
    let entity_id = play(ctx.session)?.player.entity_id;
    let animation = if packet.hand == 0 { 0 } else { 3 };
    if let Some(slot) = &ctx.session.reservation {
        let out = Clientbound::from(crate::protocol::packets::play_entities::EntityAnimation {
            id: entity_id,
            animation,
        });
        slot.broadcast_to_viewers(&out)?;
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
                player: crate::game::Player::at_spawn(
                    1,
                    uuid::Uuid::nil(),
                    "Test".into(),
                    GameMode::Spectator,
                ),
                pending_teleport: None,
                next_teleport: 1,
                pending_keep_alive: None,
                next_keep_alive: now,
                chunks: ChunkStream::new(2, now),
                blocks: blocks::CreativeSession::new(Arc::default()),
                open_container: None,
                next_window_id: 1,
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
