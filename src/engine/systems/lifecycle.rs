//! Player entity lifecycle & server control ECS handlers.
use super::{drops::spawn_drop_scatter, interaction::refresh_container_viewers};
use crate::{
    config::GameMode,
    engine::{
        components::{
            EntityUuid, NetworkEntityId, PlayerContainer, PlayerData, PlayerInventory,
            PlayerMarker, PlayerMining, PlayerViewState,
        },
        io::{ContainerKind, GameOutput},
        resources::{ChestViewers, EntityRegistry, OutputQueue, VoxelWorld},
    },
    game::{Inventory, Item, Player},
    world::{BlockPos, TimeSnapshot, WeatherEvent},
};
use bevy_ecs::prelude::World;
use std::{collections::HashSet, io, sync::mpsc::SyncSender};
use uuid::Uuid;

pub(crate) fn handle_join(ecs: &mut World, player: Player, inventory: Inventory) {
    let uuid = player.uuid;
    let entity_id = player.entity_id;
    if let Some(existing) = ecs.resource_mut::<EntityRegistry>().players.remove(&uuid) {
        ecs.despawn(existing);
    }
    let entity = ecs
        .spawn((
            PlayerMarker,
            NetworkEntityId(entity_id),
            EntityUuid(uuid),
            PlayerData(player),
            PlayerInventory(inventory),
            PlayerMining::default(),
            PlayerContainer::default(),
            PlayerViewState {
                tracked_chunks: HashSet::new(),
                pending_teleport: true,
            },
        ))
        .id();
    ecs.resource_mut::<EntityRegistry>()
        .players
        .insert(uuid, entity);
}

pub(crate) fn handle_leave(ecs: &mut World, uuid: Uuid, persist: bool) {
    let Some(entity) = ecs.resource_mut::<EntityRegistry>().players.remove(&uuid) else {
        return;
    };
    let (player, mut inventory, open_container) = {
        let Some(mut entity_mut) = ecs.get_entity_mut(entity).ok() else {
            return;
        };
        let player = entity_mut
            .get::<PlayerData>()
            .map(|p| p.0.clone())
            .expect("player data");
        let inv = entity_mut
            .get::<PlayerInventory>()
            .map(|i| i.0.clone())
            .expect("player inventory");
        let open = entity_mut
            .get_mut::<PlayerContainer>()
            .and_then(|mut c| c.open.take());
        (player, inv, open)
    };
    ecs.despawn(entity);

    let pos = player.position;
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
            spawn_drop_scatter(ecs, pos, stack, 40);
        }
    }

    let changed_chests = ecs.resource_mut::<ChestViewers>().remove_player(uuid);
    let world = ecs.resource::<VoxelWorld>().0.clone();
    {
        let mut out = ecs.resource_mut::<OutputQueue>();
        for (container_pos, count) in changed_chests {
            if world.chest_items(container_pos).is_some() {
                out.push(GameOutput::ChestLidUpdated {
                    pos: container_pos,
                    viewer_count: count,
                });
            }
        }
    }

    if persist {
        tracing::info!(
            player = %player.name,
            uuid = %uuid,
            "left the game"
        );
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PersistPlayer {
                player: Box::new(player),
                inventory: Box::new(inventory),
            });
    }
}

pub(crate) fn handle_chunk_tracked(ecs: &mut World, uuid: Uuid, chunk: [i32; 2]) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if let Some(mut view) = ecs.get_mut::<PlayerViewState>(entity) {
        view.tracked_chunks.insert(chunk);
    }
}

pub(crate) fn handle_chunks_untracked(ecs: &mut World, uuid: Uuid, chunks: HashSet<[i32; 2]>) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if let Some(mut view) = ecs.get_mut::<PlayerViewState>(entity) {
        for chunk in chunks {
            view.tracked_chunks.remove(&chunk);
        }
    }
}

pub(crate) fn handle_teleport_confirmed(ecs: &mut World, uuid: Uuid) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if let Some(mut view) = ecs.get_mut::<PlayerViewState>(entity) {
        view.pending_teleport = false;
    }
    if let Some(player) = ecs.get::<PlayerData>(entity) {
        let pose = player.0.pose();
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerPoseUpdated { uuid, pose });
    }
}

pub(crate) fn handle_respawn(ecs: &mut World, uuid: Uuid) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let world_spawn = world.spawn_position();

    let (dead, respawn_point) = {
        let Some(p) = ecs.get::<PlayerData>(entity) else {
            return;
        };
        (p.0.dead, p.0.respawn_point)
    };
    if !dead {
        return;
    }

    let mut missing_bed = false;
    let spawn = if let Some(bed_pos) = respawn_point {
        let is_bed = bed_pos.valid()
            && crate::game::registry()
                .state_by_id(world.block_at(bed_pos))
                .is_some_and(|(_, def, _)| def.name.ends_with("_bed"));
        if is_bed {
            [
                f64::from(bed_pos.0[0]) + 0.5,
                f64::from(bed_pos.0[1]) + 0.6,
                f64::from(bed_pos.0[2]) + 0.5,
            ]
        } else {
            missing_bed = true;
            world_spawn
        }
    } else {
        world_spawn
    };

    let (game_mode, health, food, saturation, pose) = {
        let mut p = ecs.get_mut::<PlayerData>(entity).expect("player data");
        if missing_bed {
            p.0.respawn_point = None;
        }
        p.0.respawn_at(spawn);
        (
            p.0.game_mode,
            p.0.health,
            p.0.food,
            p.0.saturation,
            p.0.pose(),
        )
    };
    if let Some(mut mining) = ecs.get_mut::<PlayerMining>(entity) {
        mining.0 = None;
    }
    if let Some(mut container) = ecs.get_mut::<PlayerContainer>(entity) {
        container.open = None;
    }
    if let Some(mut view) = ecs.get_mut::<PlayerViewState>(entity) {
        view.tracked_chunks.clear();
        view.pending_teleport = true;
    }
    let inventory = ecs
        .get::<PlayerInventory>(entity)
        .map(|i| i.0.clone())
        .expect("player inventory");

    let mut out = ecs.resource_mut::<OutputQueue>();
    out.push(GameOutput::PlayerPoseUpdated { uuid, pose });
    out.push(GameOutput::PlayerRespawned {
        uuid,
        spawn,
        game_mode,
        health,
        food,
        saturation,
        missing_bed,
        inventory,
    });
}

pub(crate) fn handle_control_teleport(
    ecs: &mut World,
    uuid: Uuid,
    position: [f64; 3],
    yaw: Option<f32>,
    pitch: Option<f32>,
) {
    if !Player::position_in_bounds(position) {
        return;
    }
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let (final_yaw, final_pitch) = {
        let Some(mut p) = ecs.get_mut::<PlayerData>(entity) else {
            return;
        };
        p.0.position = position;
        if let Some(y) = yaw {
            p.0.yaw = y;
        }
        if let Some(pi) = pitch {
            p.0.pitch = pi;
        }
        p.0.on_ground = false;
        (p.0.yaw, p.0.pitch)
    };
    if let Some(mut view) = ecs.get_mut::<PlayerViewState>(entity) {
        view.pending_teleport = true;
    }
    ecs.resource_mut::<OutputQueue>()
        .push(GameOutput::PlayerTeleported {
            uuid,
            position,
            yaw: final_yaw,
            pitch: final_pitch,
        });
}

pub(crate) fn handle_control_give(ecs: &mut World, uuid: Uuid, item: Item, count: u32) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let inventory = {
        let Some(mut inv) = ecs.get_mut::<PlayerInventory>(entity) else {
            return;
        };
        inv.0.give(item, count);
        inv.0.clone()
    };
    ecs.resource_mut::<OutputQueue>()
        .push(GameOutput::PlayerInventorySynced { uuid, inventory });
}

pub(crate) fn handle_control_game_mode(ecs: &mut World, uuid: Uuid, mode: GameMode) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let (health, food, saturation) = {
        let Some(mut p) = ecs.get_mut::<PlayerData>(entity) else {
            return;
        };
        p.0.game_mode = mode;
        (p.0.health, p.0.food, p.0.saturation)
    };
    let inventory = ecs
        .get::<PlayerInventory>(entity)
        .map(|i| i.0.clone())
        .expect("player inventory");
    ecs.resource_mut::<OutputQueue>()
        .push(GameOutput::PlayerGameModeChanged {
            uuid,
            mode,
            health,
            food,
            saturation,
            inventory,
        });
}

fn reconcile_container_at(ecs: &mut World, pos: BlockPos) {
    if !ecs.resource::<ChestViewers>().0.contains_key(&pos) {
        return;
    }
    refresh_container_viewers(ecs, pos, Uuid::nil());
    let world = ecs.resource::<VoxelWorld>().0.clone();
    if world.chest_items(pos).is_none() && world.furnace_data(pos).is_none() {
        ecs.resource_mut::<ChestViewers>().0.remove(&pos);
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::ChestLidUpdated {
                pos,
                viewer_count: 0,
            });
    }
}

pub(crate) fn handle_control_set_block(
    ecs: &mut World,
    pos: BlockPos,
    state: i32,
    reply: Option<SyncSender<bool>>,
) {
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let changed = if let Some(mutation) = world.set_block(pos, state) {
        reconcile_container_at(ecs, pos);
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::WorldMutated(mutation));
        true
    } else {
        false
    };
    if let Some(tx) = reply {
        let _ = tx.send(changed);
    }
}

pub(crate) fn handle_control_fill_blocks(
    ecs: &mut World,
    from: BlockPos,
    to: BlockPos,
    state: i32,
    reply: Option<SyncSender<io::Result<usize>>>,
) {
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let result = match world.fill_blocks(from, to, state) {
        Ok(mutation) => {
            let count = mutation.block_changes.len();
            if !ecs.resource::<ChestViewers>().0.is_empty() {
                let positions: Vec<BlockPos> =
                    mutation.block_changes.iter().map(|&(pos, _)| pos).collect();
                for pos in positions {
                    reconcile_container_at(ecs, pos);
                }
            }
            ecs.resource_mut::<OutputQueue>()
                .push(GameOutput::WorldMutated(mutation));
            Ok(count)
        }
        Err(error) => Err(error),
    };
    if let Some(tx) = reply {
        let _ = tx.send(result);
    }
}

pub(crate) fn handle_control_set_day_time(
    ecs: &mut World,
    day_time: i64,
    reply: Option<SyncSender<TimeSnapshot>>,
) {
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let snap = world.set_day_time(day_time);
    ecs.resource_mut::<OutputQueue>()
        .push(GameOutput::TimeUpdated(snap));
    if let Some(tx) = reply {
        let _ = tx.send(snap);
    }
}

pub(crate) fn handle_control_set_weather(
    ecs: &mut World,
    raining: bool,
    thundering: bool,
    duration: u32,
    reply: Option<SyncSender<Vec<WeatherEvent>>>,
) {
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let events = world.set_weather(raining, thundering, duration);
    {
        let mut out = ecs.resource_mut::<OutputQueue>();
        for &event in &events {
            out.push(GameOutput::WeatherChanged(event));
        }
    }
    if let Some(tx) = reply {
        let _ = tx.send(events);
    }
}
