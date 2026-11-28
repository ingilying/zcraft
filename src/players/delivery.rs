//! Wire delivery items and server-initiated player control messages.
use super::Chunk;
use crate::protocol::{Clientbound, packets::play_entities::*};
use std::{io, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) struct Peer {
    pub uuid: Uuid,
    pub name: Arc<str>,
    pub id: i32,
    pub game_mode: crate::config::GameMode,
    pub pose: EntityPose,
    pub info: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub(crate) enum PlayerControl {
    SyncTeleport {
        position: [f64; 3],
        yaw: f32,
        pitch: f32,
    },
    SyncRespawn {
        spawn: [f64; 3],
        game_mode: crate::config::GameMode,
        health: f32,
        food: i32,
        saturation: f32,
        missing_bed: bool,
        inventory: Box<crate::game::Inventory>,
    },
    RefreshCommands,
    Kick(String),
}

#[derive(Debug)]
pub(crate) enum Delivery {
    Packet(Arc<[u8]>),
    Control(PlayerControl),
    Chat {
        body: Arc<[u8]>,
        private: bool,
    },
    Terrain {
        chunk: Chunk,
        updates: Arc<[Arc<[u8]>]>,
        ack: Option<Arc<[u8]>>,
    },
    Add {
        peer: Peer,
        spawn: bool,
    },
    Spawn(SpawnPlayer),
    Despawn(i32),
    Remove {
        uuid: Uuid,
        id: Option<i32>,
    },
    Move {
        id: i32,
        from: EntityPose,
        to: EntityPose,
    },
}

impl Delivery {
    pub(crate) fn bodies(self) -> io::Result<impl Iterator<Item = Arc<[u8]>> + Send> {
        let mut bodies = [None, None];
        let encode = |packet: Clientbound| packet.encode().map(Arc::<[u8]>::from);
        let mut terrain = None;
        let mut ack = None;
        match self {
            Self::Packet(body) | Self::Chat { body, .. } => bodies[0] = Some(body),
            Self::Control(_) => {}
            Self::Terrain {
                updates,
                ack: sequence,
                ..
            } => {
                terrain = Some(updates);
                ack = sequence;
            }
            Self::Add { peer, spawn } => {
                bodies[0] = Some(peer.info);
                if spawn {
                    bodies[1] = Some(encode(
                        SpawnEntity::Player(SpawnPlayer {
                            id: peer.id,
                            uuid: peer.uuid,
                            pose: peer.pose,
                        })
                        .into(),
                    )?);
                }
            }
            Self::Spawn(packet) => bodies[0] = Some(encode(SpawnEntity::Player(packet).into())?),
            Self::Despawn(id) => bodies[0] = Some(encode(RemoveEntities(vec![id]).into())?),
            Self::Remove { uuid, id } => {
                if let Some(id) = id {
                    bodies[0] = Some(encode(RemoveEntities(vec![id]).into())?);
                }
                bodies[1] = Some(encode(RemovePlayerInfo(vec![uuid]).into())?);
            }
            Self::Move { id, from, to } => {
                let packet = if from.position == to.position {
                    EntityLook { id, pose: to }.into()
                } else {
                    EntityTeleport { id, pose: to }.into()
                };
                bodies[0] = Some(encode(packet)?);
                if from.yaw != to.yaw {
                    bodies[1] = Some(encode(EntityHeadRotation { id, yaw: to.yaw }.into())?);
                }
            }
        }
        let mut index = 0;
        let updates = std::iter::from_fn(move || {
            let body = terrain.as_ref()?.get(index)?.clone();
            index += 1;
            Some(body)
        });
        Ok(bodies.into_iter().flatten().chain(updates).chain(ack))
    }
}
