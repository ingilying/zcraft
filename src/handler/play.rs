//! Minimal, read-only spectator world. No authoritative gameplay or persistence yet.
mod chunks;

use self::chunks::ChunkStream;
use super::{Context, Outcome, Session};
use crate::{
    players::Peer,
    protocol::{
        Clientbound, invalid,
        packets::{play::*, play_common::*, play_entities::EntityPose},
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
    max_view_distance: i32,
    next_entity: AtomicI32,
}
impl PlayService {
    pub fn new() -> io::Result<Self> {
        Self::with_max_view_distance(world::VIEW_DISTANCE)
    }

    pub fn with_max_view_distance(max_view_distance: i32) -> io::Result<Self> {
        world::validate_view_distance(max_view_distance)?;
        Ok(Self {
            terrain: world::chunk_payload(),
            max_view_distance,
            next_entity: AtomicI32::new(1),
        })
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
    pub position: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    on_ground: bool,
    pending_teleport: Option<(i32, Instant)>,
    next_teleport: i32,
    pending_keep_alive: Option<(i64, Instant)>,
    next_keep_alive: Instant,
    chunks: ChunkStream,
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
    let first_batch = chunks.pump(&ctx.server.play.terrain, now);
    ctx.session.play = Some(PlaySession {
        entity_id,
        position: [world::SPAWN_X, world::SPAWN_Y, world::SPAWN_Z],
        yaw: 0.0,
        pitch: 0.0,
        on_ground: false,
        pending_teleport: Some((0, now)),
        next_teleport: 1,
        pending_keep_alive: None,
        next_keep_alive: now + KEEP_ALIVE_INTERVAL,
        chunks,
    });
    let mut replies = vec![
        JoinGame {
            entity_id,
            view_distance,
        }
        .into(),
        PlayerInfo { profile }.into(),
        PlayerAbilities.into(),
        DefaultSpawnPosition.into(),
        TimeUpdate.into(),
        WorldBorder.into(),
        CenterChunk([0, 0]).into(),
        GameEvent.into(),
    ];
    replies.extend(first_batch);
    // The nearest chunks (including spawn) precede the initial teleport.
    // The rest of the view streams independently of teleport confirmation.
    replies.push(PlayerPosition { teleport_id: 0 }.into());
    Ok(Outcome {
        replies,
        close: false,
        encryption: None,
    })
}

fn play(session: &mut Session) -> io::Result<&mut PlaySession> {
    session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))
}

/// The network reports completed writes; response deadlines start when the
/// request actually leaves the output batch, not when that batch was built.
pub fn packet_sent(session: &mut Session, packet: &crate::protocol::Clientbound, now: Instant) {
    let Some(state) = session.play.as_mut() else {
        return;
    };
    state.chunks.packet_sent(packet, now);
    if let Clientbound::ChunkData(chunk) = packet
        && let Some(slot) = &session.reservation
    {
        slot.chunk_sent(chunk.0);
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
            slot.show(Peer {
                uuid: profile.uuid,
                id: entity_id,
                pose,
                info: Clientbound::from(PlayerInfo {
                    profile: profile.clone(),
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
}

fn movement(
    ctx: &mut Context<'_>,
    position: Option<[f64; 3]>,
    rotation: Option<[f32; 2]>,
    on_ground: bool,
) -> io::Result<Outcome> {
    let state = play(ctx.session)?;
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
            let view = state.chunks.view();
            state.yaw = 0.0;
            state.pitch = 0.0;
            state.on_ground = false;
            if let Some(slot) = &ctx.session.reservation {
                slot.set_view(view.0, view.1);
            }
            return Ok(Outcome {
                replies: vec![PlayerPosition { teleport_id: id }.into()],
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
    let view = state.chunks.view();
    if let Some(slot) = &ctx.session.reservation {
        if position.is_some() {
            slot.set_view(view.0, view.1);
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
    let packets = play(session)?.chunks.pump(&service.terrain, now);
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
    let chunks = &mut play(ctx.session)?.chunks;
    chunks.set_radius(distance);
    let (center, radius) = chunks.view();
    if let Some(slot) = &ctx.session.reservation {
        slot.set_view(center, radius);
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
                position: [0.0; 3],
                yaw: 0.0,
                pitch: 0.0,
                on_ground: false,
                pending_teleport: None,
                next_teleport: 1,
                pending_keep_alive: None,
                next_keep_alive: now,
                chunks: ChunkStream::new(2, now),
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
