//! Protocol-level connection control actions (teleport ID/chunk-stream sync,
//! respawn chunk-stream reset, command tree refresh, kick).
//! All gameplay mutations happen in the ECS engine; this module only synchronizes
//! transport/session protocol state.
use super::{Outcome, PlayService, Session, blocks, chunks::ChunkStream, closed};
use crate::{
    config::GameMode,
    players::PlayerControl,
    protocol::{
        invalid,
        packets::{
            play::{
                CenterChunk, ChangeDifficulty, DefaultSpawnPosition, GameEvent, PlayerAbilities,
                PlayerPosition, Respawn, UpdateHealth,
            },
            play_blocks::{InventoryContent, SelectedSlot},
            play_chat::DeclareCommands,
        },
    },
    world,
};
use std::io;
use tokio::time::Instant;

pub(crate) async fn apply_control(
    session: &mut Session,
    service: &PlayService,
    players: &crate::players::Players,
    control: PlayerControl,
) -> io::Result<Outcome> {
    let slot = session.reservation.as_ref();
    let state = session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))?;
    match control {
        PlayerControl::SyncTeleport {
            position,
            yaw,
            pitch,
        } => {
            let now = Instant::now();
            let id = state.next_teleport;
            state.next_teleport = id
                .checked_add(1)
                .ok_or_else(|| invalid("teleport ID exhausted"))?;
            state.pending_teleport = Some((id, now));
            let dest_chunk = world::chunk_at(position[0], position[2]);
            let mut replies = Vec::new();
            if !state.chunks.is_loaded(dest_chunk) {
                let world = service.world().clone();
                let (unloaded, mut chunk_packets) =
                    state
                        .chunks
                        .prepare_teleport(dest_chunk, world.terrain(), now);
                let (center, radius) = state.chunks.view();
                if let Some(slot) = slot {
                    slot.set_view(center, radius);
                    if !unloaded.is_empty() {
                        slot.retire_blocks(&unloaded);
                        replies.extend(slot.before_unload(&unloaded));
                    }
                }
                if !unloaded.is_empty() {
                    service.dispatch_input(
                        crate::engine::GameInput::PlayerChunksUntracked {
                            uuid: state.uuid,
                            chunks: unloaded,
                        },
                        players,
                    )?;
                }
                state.blocks.prepare_chunks_async(&mut chunk_packets).await?;
                replies.extend(chunk_packets);
            } else {
                let old_view = state.chunks.view();
                state.chunks.follow(position[0], position[2]);
                if let Some(slot) = slot
                    && old_view != state.chunks.view()
                {
                    state.update_view(slot, service, players)?;
                }
            }
            replies.push(PlayerPosition::at(position, yaw, pitch, id).into());
            Ok(Outcome {
                replies,
                close: false,
                encryption: None,
            })
        }
        PlayerControl::SyncRespawn {
            spawn,
            game_mode,
            health,
            food,
            saturation,
            missing_bed,
            inventory,
        } => {
            let world = service.world().clone();
            let world_spawn = world.spawn_position();
            let is_flat = world.is_flat();
            let view_distance = service.view_distance(session.client_information.as_ref());
            let now = Instant::now();

            let tp_id = state.next_teleport;
            state.next_teleport = tp_id
                .checked_add(1)
                .ok_or_else(|| invalid("teleport ID exhausted"))?;
            state.pending_teleport = Some((tp_id, now));

            let spawn_chunk = world::chunk_at(spawn[0], spawn[2]);
            state.chunks = ChunkStream::at(spawn_chunk, view_distance, now);
            state.blocks = blocks::CreativeSession::new(world.clone());
            let mut first_batch = state.chunks.pump(world.terrain(), now);
            state.blocks.prepare_chunks_async(&mut first_batch).await?;

            if let Some(slot) = slot {
                slot.set_view(spawn_chunk, view_distance);
            }
            let mut replies = Vec::new();
            if missing_bed {
                replies.push(GameEvent::no_respawn_block_available().into());
            }
            replies.extend([
                Respawn { game_mode, is_flat }.into(),
                DefaultSpawnPosition::from_spawn(world_spawn).into(),
                ChangeDifficulty::normal().into(),
                PlayerAbilities(game_mode).into(),
                UpdateHealth::new(health, food, saturation).into(),
                CenterChunk(spawn_chunk).into(),
                GameEvent::level_chunks_load_start().into(),
            ]);
            if game_mode != GameMode::Spectator {
                replies.push(InventoryContent::player(&inventory).into());
                replies.push(SelectedSlot(inventory.selected()).into());
            }
            replies.extend(first_batch);
            replies.extend(service.existing_drop_packets_async().await);
            replies.push(PlayerPosition::at(spawn, 0.0, 0.0, tp_id).into());
            Ok(Outcome {
                replies,
                close: false,
                encryption: None,
            })
        }
        PlayerControl::RefreshCommands => {
            let profile = session
                .profile
                .as_ref()
                .ok_or_else(|| invalid("missing authenticated identity"))?;
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
        PlayerControl::Kick(reason) => Ok(closed(&reason)),
    }
}
