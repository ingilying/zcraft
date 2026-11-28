//! Declarative ECS systems for player entity lifecycle, view state, and
//! server/world control commands.
use crate::{
    game::{
        Player,
        components::{
            Dead, EatingState, EntityUuid, Health, Hunger, MovementState, NetworkEntityId,
            PlayerContainer, PlayerInventory, PlayerMarker, PlayerProfile, PlayerViewState,
            Position, Rotation, entity_pose,
        },
        events::{
            PlayerControlMsg, PlayerJoinMsg, PlayerLeaveMsg, PlayerViewMsg, SpawnDropMsg,
            WorldControlMsg,
        },
        io::{ContainerKind, GameOutput, OpenContainer},
        resources::{ChestViewers, EntityRegistry, OutputQueue, VoxelWorld},
    },
    world::BlockPos,
};
use bevy_ecs::prelude::{Commands, MessageReader, MessageWriter, Query, Res, ResMut, With};
use std::collections::HashSet;
use uuid::Uuid;

/// Spawn online player entities from `PlayerJoinMsg`.
pub(crate) fn player_join_system(
    mut joins: MessageReader<PlayerJoinMsg>,
    mut commands: Commands,
    mut registry: ResMut<EntityRegistry>,
) {
    for PlayerJoinMsg::Join { player, inventory } in joins.read() {
        let player = player.as_ref();
        let uuid = player.uuid;
        let entity_id = player.entity_id;
        if let Some(existing) = registry.players.remove(&uuid) {
            commands.entity(existing).despawn();
        }
        let mut entity_cmd = commands.spawn((
            PlayerMarker,
            NetworkEntityId(entity_id),
            EntityUuid(uuid),
            PlayerProfile {
                name: player.name.clone(),
                game_mode: player.game_mode,
                respawn_point: player.respawn_point,
            },
            Position(player.position),
            Rotation {
                yaw: player.yaw,
                pitch: player.pitch,
            },
            MovementState {
                on_ground: player.on_ground,
                sneaking: player.sneaking,
                sprinting: player.sprinting,
                fall_distance: player.fall_distance,
            },
            Health {
                current: player.health,
                last_hurt_tick: player.last_hurt_tick,
            },
            Hunger {
                food: player.food,
                saturation: player.saturation,
                exhaustion: player.exhaustion,
                tick_timer: player.food_tick_timer,
            },
            PlayerInventory(inventory.as_ref().clone()),
            PlayerContainer::default(),
            PlayerViewState {
                tracked_chunks: HashSet::new(),
                pending_teleport: true,
            },
        ));
        if player.dead {
            entity_cmd.insert(Dead);
        }
        if let Some((hand, item, start_tick, required_ticks)) = player.eating {
            entity_cmd.insert(EatingState {
                hand,
                item,
                start_tick,
                required_ticks,
            });
        }
        let entity = entity_cmd.id();
        registry.players.insert(uuid, entity);
    }
}

/// Apply transport-edge chunk tracking and teleport confirmation updates.
pub(crate) fn player_view_system(
    mut views: MessageReader<PlayerViewMsg>,
    registry: Res<EntityRegistry>,
    mut players: Query<(&Position, &Rotation, &MovementState, &mut PlayerViewState), With<PlayerMarker>>,
    mut out: ResMut<OutputQueue>,
) {
    for msg in views.read() {
        match msg {
            PlayerViewMsg::ChunkTracked { uuid, chunk } => {
                let Some(&entity) = registry.players.get(uuid) else {
                    continue;
                };
                if let Ok((_, _, _, mut view)) = players.get_mut(entity) {
                    view.tracked_chunks.insert(*chunk);
                }
            }
            PlayerViewMsg::ChunksUntracked { uuid, chunks } => {
                let Some(&entity) = registry.players.get(uuid) else {
                    continue;
                };
                if let Ok((_, _, _, mut view)) = players.get_mut(entity) {
                    for chunk in chunks {
                        view.tracked_chunks.remove(chunk);
                    }
                }
            }
            PlayerViewMsg::TeleportConfirmed { uuid } => {
                let Some(&entity) = registry.players.get(uuid) else {
                    continue;
                };
                if let Ok((&pos, &rot, &movement, mut view)) = players.get_mut(entity) {
                    view.pending_teleport = false;
                    let pose = entity_pose(pos, rot, movement);
                    out.push(GameOutput::PlayerPoseUpdated { uuid: *uuid, pose });
                }
            }
        }
    }
}

/// Apply server/admin player controls (`/tp`, `/give`, `/gamemode`).
pub(crate) fn player_control_system(
    mut controls: MessageReader<PlayerControlMsg>,
    registry: Res<EntityRegistry>,
    mut players: Query<
        (
            &mut Position,
            &mut Rotation,
            &mut MovementState,
            &mut PlayerProfile,
            &Health,
            &Hunger,
            &mut PlayerInventory,
            &mut PlayerViewState,
        ),
        With<PlayerMarker>,
    >,
    mut out: ResMut<OutputQueue>,
) {
    for msg in controls.read() {
        match *msg {
            PlayerControlMsg::Teleport {
                uuid,
                position,
                yaw,
                pitch,
            } => {
                if !Player::position_in_bounds(position) {
                    continue;
                }
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                let Ok((mut pos, mut rot, mut movement, _, _, _, _, mut view)) =
                    players.get_mut(entity)
                else {
                    continue;
                };
                pos.0 = position;
                if let Some(y) = yaw {
                    rot.yaw = y;
                }
                if let Some(pi) = pitch {
                    rot.pitch = pi;
                }
                movement.on_ground = false;
                view.pending_teleport = true;
                out.push(GameOutput::PlayerTeleported {
                    uuid,
                    position,
                    yaw: rot.yaw,
                    pitch: rot.pitch,
                });
            }
            PlayerControlMsg::Give { uuid, item, count } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                let Ok((_, _, _, _, _, _, mut inv, _)) = players.get_mut(entity) else {
                    continue;
                };
                inv.0.give(item, count);
                out.push(GameOutput::PlayerInventorySynced {
                    uuid,
                    inventory: inv.0.clone(),
                });
            }
            PlayerControlMsg::SetGameMode { uuid, mode } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                let Ok((_, _, _, mut profile, health, hunger, inv, _)) = players.get_mut(entity)
                else {
                    continue;
                };
                profile.game_mode = mode;
                out.push(GameOutput::PlayerGameModeChanged {
                    uuid,
                    mode,
                    health: health.current,
                    food: hunger.food,
                    saturation: hunger.saturation,
                    inventory: inv.0.clone(),
                });
            }
        }
    }
}

fn reconcile_container_at(
    pos: BlockPos,
    world: &VoxelWorld,
    viewers: &mut ChestViewers,
    registry: &EntityRegistry,
    players: &mut Query<(&mut PlayerContainer, &PlayerInventory), With<PlayerMarker>>,
    out: &mut OutputQueue,
) {
    if !viewers.0.contains_key(&pos) {
        return;
    }
    let others = viewers.others(pos, Uuid::nil());
    let chest_items = world.0.chest_items(pos);
    let furnace_data = world.0.furnace_data(pos);

    for viewer_uuid in others {
        let Some(&entity) = registry.players.get(&viewer_uuid) else {
            continue;
        };
        let Ok((mut container, inv)) = players.get_mut(entity) else {
            continue;
        };
        let Some(open) = container.open.clone() else {
            continue;
        };
        match open.kind {
            ContainerKind::Chest(cpos) if cpos == pos => {
                if let Some(items) = &chest_items {
                    let next_state_id = open.state_id.wrapping_add(1);
                    container.open = Some(OpenContainer {
                        state_id: next_state_id,
                        ..open
                    });
                    let slots = crate::game::containers::container_slots(items, &inv.0);
                    out.push(GameOutput::ContainerSynced {
                        uuid: viewer_uuid,
                        window_id: open.window_id,
                        state_id: next_state_id,
                        slots,
                        carried: inv.0.carried(),
                        furnace_props: None,
                    });
                } else {
                    container.open = None;
                    out.push(GameOutput::ContainerClosed {
                        uuid: viewer_uuid,
                        window_id: open.window_id,
                    });
                }
            }
            ContainerKind::Furnace(fpos) if fpos == pos => {
                if let Some(furnace) = &furnace_data {
                    let next_state_id = open.state_id.wrapping_add(1);
                    container.open = Some(OpenContainer {
                        state_id: next_state_id,
                        ..open
                    });
                    let slots = crate::game::containers::furnace_slots(furnace, &inv.0);
                    let props = [
                        furnace.burn_time as i16,
                        furnace.burn_total as i16,
                        furnace.cook_time as i16,
                        furnace.cook_total as i16,
                    ];
                    out.push(GameOutput::ContainerSynced {
                        uuid: viewer_uuid,
                        window_id: open.window_id,
                        state_id: next_state_id,
                        slots,
                        carried: inv.0.carried(),
                        furnace_props: Some(props),
                    });
                } else {
                    container.open = None;
                    out.push(GameOutput::ContainerClosed {
                        uuid: viewer_uuid,
                        window_id: open.window_id,
                    });
                }
            }
            _ => {}
        }
    }

    if chest_items.is_none() && furnace_data.is_none() {
        viewers.0.remove(&pos);
        out.push(GameOutput::ChestLidUpdated {
            pos,
            viewer_count: 0,
        });
    }
}

/// Apply authoritative world commands (`/setblock`, `/fill`, `/time`, `/weather`).
pub(crate) fn world_control_system(
    mut controls: MessageReader<WorldControlMsg>,
    world: Res<VoxelWorld>,
    mut viewers: ResMut<ChestViewers>,
    registry: Res<EntityRegistry>,
    mut players: Query<(&mut PlayerContainer, &PlayerInventory), With<PlayerMarker>>,
    mut out: ResMut<OutputQueue>,
) {
    for msg in controls.read() {
        match msg {
            WorldControlMsg::SetBlock { pos, state, reply } => {
                let changed = if let Some(mutation) = world.0.set_block(*pos, *state) {
                    reconcile_container_at(
                        *pos,
                        &world,
                        &mut viewers,
                        &registry,
                        &mut players,
                        &mut out,
                    );
                    out.push(GameOutput::WorldMutated(mutation));
                    true
                } else {
                    false
                };
                if let Some(tx) = reply {
                    let _ = tx.send(changed);
                }
            }
            WorldControlMsg::FillBlocks {
                from,
                to,
                state,
                reply,
            } => {
                let result = match world.0.fill_blocks(*from, *to, *state) {
                    Ok(mutation) => {
                        let count = mutation.block_changes.len();
                        if !viewers.0.is_empty() {
                            for &(pos, _) in &mutation.block_changes {
                                reconcile_container_at(
                                    pos,
                                    &world,
                                    &mut viewers,
                                    &registry,
                                    &mut players,
                                    &mut out,
                                );
                            }
                        }
                        out.push(GameOutput::WorldMutated(mutation));
                        Ok(count)
                    }
                    Err(error) => Err(error),
                };
                if let Some(tx) = reply {
                    let _ = tx.send(result);
                }
            }
            WorldControlMsg::SetDayTime { day_time, reply } => {
                let snap = world.0.set_day_time(*day_time);
                out.push(GameOutput::TimeUpdated(snap));
                if let Some(tx) = reply {
                    let _ = tx.send(snap);
                }
            }
            WorldControlMsg::SetWeather {
                raining,
                thundering,
                duration,
                reply,
            } => {
                let events = world.0.set_weather(*raining, *thundering, *duration);
                for &event in &events {
                    out.push(GameOutput::WeatherChanged(event));
                }
                if let Some(tx) = reply {
                    let _ = tx.send(events);
                }
            }
        }
    }
}

/// Despawn departing players, return crafting/cursor items, close chest viewers,
/// and emit `PersistPlayer` events.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn player_leave_system(
    mut leaves: MessageReader<PlayerLeaveMsg>,
    mut commands: Commands,
    world: Res<VoxelWorld>,
    mut registry: ResMut<EntityRegistry>,
    mut viewers: ResMut<ChestViewers>,
    mut players: Query<
        (
            &NetworkEntityId,
            &EntityUuid,
            &PlayerProfile,
            &Position,
            &Rotation,
            &MovementState,
            &Health,
            &Hunger,
            Option<&Dead>,
            Option<&EatingState>,
            &PlayerInventory,
            &mut PlayerContainer,
        ),
        With<PlayerMarker>,
    >,
    mut drops: MessageWriter<SpawnDropMsg>,
    mut out: ResMut<OutputQueue>,
) {
    for &PlayerLeaveMsg::Leave { uuid, persist } in leaves.read() {
        let Some(entity) = registry.players.remove(&uuid) else {
            continue;
        };
        let Ok((
            net_id,
            entity_uuid,
            profile,
            pos,
            rot,
            movement,
            health,
            hunger,
            dead,
            eating,
            inv_comp,
            mut container,
        )) = players.get_mut(entity)
        else {
            continue;
        };

        let player = Player {
            entity_id: net_id.0,
            uuid: entity_uuid.0,
            name: profile.name.clone(),
            game_mode: profile.game_mode,
            position: pos.0,
            yaw: rot.yaw,
            pitch: rot.pitch,
            on_ground: movement.on_ground,
            sneaking: movement.sneaking,
            sprinting: movement.sprinting,
            health: health.current,
            food: hunger.food,
            saturation: hunger.saturation,
            exhaustion: hunger.exhaustion,
            food_tick_timer: hunger.tick_timer,
            eating: eating.map(|e| (e.hand, e.item, e.start_tick, e.required_ticks)),
            fall_distance: movement.fall_distance,
            dead: dead.is_some(),
            last_hurt_tick: health.last_hurt_tick,
            respawn_point: profile.respawn_point,
        };
        let mut inventory = inv_comp.0.clone();
        let open_container = container.open.take();
        commands.entity(entity).despawn();

        let ppos = player.position;
        let mut returned = Vec::new();
        if let Some(carried) = inventory.take_carried() {
            returned.push(carried);
        }
        returned.extend(inventory.drain_crafting_grid());
        if let Some(open) = open_container
            && let ContainerKind::CraftingTable { grid, .. } = open.kind
        {
            returned.extend(grid.into_iter().flatten());
        }
        for mut stack in returned {
            let added = inventory.give_stack(stack);
            if added < stack.count {
                stack.count -= added;
                drops.write(SpawnDropMsg::scatter(ppos, stack, 40));
            }
        }

        for (container_pos, count) in viewers.remove_player(uuid) {
            if world.0.chest_items(container_pos).is_some() {
                out.push(GameOutput::ChestLidUpdated {
                    pos: container_pos,
                    viewer_count: count,
                });
            }
        }

        if persist {
            tracing::info!(
                player = %player.name,
                uuid = %uuid,
                "left the game"
            );
            out.push(GameOutput::PersistPlayer {
                player: Box::new(player),
                inventory: Box::new(inventory),
            });
        }
    }
}
