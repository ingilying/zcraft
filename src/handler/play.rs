//! Minimal, read-only spectator world. No authoritative gameplay or persistence yet.
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
    chunks: Vec<Arc<[u8]>>,
    next_entity: AtomicI32,
}
impl PlayService {
    pub fn new() -> io::Result<Self> {
        let mut chunks = Vec::new();
        for z in -world::VIEW_DISTANCE..=world::VIEW_DISTANCE {
            for x in -world::VIEW_DISTANCE..=world::VIEW_DISTANCE {
                chunks.push(world::chunk_data(x, z)?.into());
            }
        }
        Ok(Self {
            chunks,
            next_entity: AtomicI32::new(1),
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
    awaiting_chunks: bool,
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
        awaiting_chunks: true,
    });
    let mut replies = vec![
        JoinGame { entity_id }.into(),
        PlayerInfo { profile }.into(),
        PlayerAbilities.into(),
        DefaultSpawnPosition.into(),
        TimeUpdate.into(),
        WorldBorder.into(),
        CenterChunk.into(),
        GameEvent.into(),
        ChunkBatchStart.into(),
    ];
    replies.extend(
        ctx.server
            .play
            .chunks
            .iter()
            .cloned()
            .map(|body| ChunkData(body).into()),
    );
    replies.push(
        ChunkBatchFinished {
            count: ctx.server.play.chunks.len() as i32,
        }
        .into(),
    );
    // Request confirmation only after all initial chunks have been written.
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
        // Keep clients within the preloaded static terrain instead of accepting
        // unlimited movement into chunks that this prototype cannot stream yet.
        if !(-15.0..=32.0).contains(&x)
            || !(-15.0..=32.0).contains(&z)
            || !(-32.0..=256.0).contains(&y)
        {
            let id = state.next_teleport;
            state.next_teleport = id
                .checked_add(1)
                .ok_or_else(|| invalid("teleport ID exhausted"))?;
            state.pending_teleport = Some((id, Instant::now()));
            state.position = [world::SPAWN_X, world::SPAWN_Y, world::SPAWN_Z];
            state.yaw = 0.0;
            state.pitch = 0.0;
            state.on_ground = false;
            return Ok(Outcome {
                replies: vec![PlayerPosition { teleport_id: id }.into()],
                close: false,
                encryption: None,
            });
        }
        state.position = [x, y, z];
    }
    if let Some([yaw, pitch]) = rotation {
        state.yaw = yaw;
        state.pitch = pitch;
    }
    state.on_ground = on_ground;
    let pose = state.pose();
    if let Some(slot) = &ctx.session.reservation {
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
pub async fn handle_batch(ctx: &mut Context<'_>, _p: ChunkBatchReceived) -> io::Result<Outcome> {
    let state = play(ctx.session)?;
    if !state.awaiting_chunks {
        return Err(invalid("unexpected chunk batch feedback"));
    }
    state.awaiting_chunks = false;
    Ok(Outcome::empty())
}
pub async fn handle_information(
    ctx: &mut Context<'_>,
    packet: PlayClientInformation,
) -> io::Result<Outcome> {
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
                awaiting_chunks: false,
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
