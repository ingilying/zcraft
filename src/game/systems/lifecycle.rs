//! Player entity lifecycle & server control ECS handlers.
use super::{drops::spawn_drop_scatter, interaction::refresh_container_viewers};
use crate::{
    config::GameMode,
    game::{
        Inventory, Item, Player,
        components::{
            Dead, EatingState, EntityUuid, FallDistance, Health, Hunger, MiningState,
            MovementFlags, NetworkEntityId, PlayerContainer, PlayerGameMode, PlayerInventory,
            PlayerMarker, PlayerName, PlayerViewState, Position, RespawnPoint, Rotation,
            entity_pose, snapshot_player_dto,
        },
        io::{ContainerKind, GameOutput},
        resources::{ChestViewers, EntityRegistry, OutputQueue, VoxelWorld},
    },
    world::{BlockPos, TimeSnapshot, WeatherEvent},
};
use bevy_ecs::prelude::World;
use std::{collections::HashSet, io, sync::mpsc::SyncSender};
use uuid::Uuid;

pub(crate) fn handle_join(ecs: &mut World, player: Player, inventory: Inventory) {
    let uuid = player.uuid;
    let entity_id = player.entity_id;
    let dead = player.dead;
    let eating = player.eating;
    if let Some(existing) = ecs.resource_mut::<EntityRegistry>().players.remove(&uuid) {
        ecs.despawn(existing);
    }
    let mut entity_mut = ecs.spawn((
        PlayerMarker,
        NetworkEntityId(entity_id),
        EntityUuid(uuid),
        PlayerName(player.name),
        PlayerGameMode(player.game_mode),
        Position(player.position),
        Rotation {
            yaw: player.yaw,
            pitch: player.pitch,
        },
        MovementFlags {
            on_ground: player.on_ground,
            sneaking: player.sneaking,
            sprinting: player.sprinting,
        },
        FallDistance(player.fall_distance),
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
        RespawnPoint(player.respawn_point),
        PlayerInventory(inventory),
        PlayerContainer::default(),
        PlayerViewState {
            tracked_chunks: HashSet::new(),
            pending_teleport: true,
        },
    ));
    if dead {
        entity_mut.insert(Dead);
    }
    if let Some((hand, item, start_tick, required_ticks)) = eating {
        entity_mut.insert(EatingState {
            hand,
            item,
            start_tick,
            required_ticks,
        });
    }
    let entity = entity_mut.id();
    ecs.resource_mut::<EntityRegistry>()
        .players
        .insert(uuid, entity);
}

pub(crate) fn handle_leave(ecs: &mut World, uuid: Uuid, persist: bool) {
    let Some(entity) = ecs.resource_mut::<EntityRegistry>().players.remove(&uuid) else {
        return;
    };
    let Some(player) = snapshot_player_dto(ecs, entity) else {
        return;
    };
    let (mut inventory, open_container) = {
        let Some(mut entity_mut) = ecs.get_entity_mut(entity).ok() else {
            return;
        };
        let inv = entity_mut
            .get::<PlayerInventory>()
            .map(|i| i.0.clone())
            .expect("player inventory");
        let open = entity_mut
            .get_mut::<PlayerContainer>()
            .and_then(|mut c| c.open.take());
        (inv, open)
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
    if let (Some(&pos), Some(&rot), Some(&flags)) = (
        ecs.get::<Position>(entity),
        ecs.get::<Rotation>(entity),
        ecs.get::<MovementFlags>(entity),
    ) {
        let pose = entity_pose(pos, rot, flags);
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerPoseUpdated { uuid, pose });
    }
}

pub(crate) fn handle_respawn(ecs: &mut World, uuid: Uuid) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if ecs.get::<Dead>(entity).is_none() {
        return;
    }
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let world_spawn = world.spawn_position();
    let respawn_point = ecs.get::<RespawnPoint>(entity).and_then(|r| r.0);

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

    if missing_bed && let Some(mut rp) = ecs.get_mut::<RespawnPoint>(entity) {
        rp.0 = None;
    }
    if let Some(mut pos) = ecs.get_mut::<Position>(entity) {
        pos.0 = spawn;
    }
    if let Some(mut rot) = ecs.get_mut::<Rotation>(entity) {
        *rot = Rotation::default();
    }
    if let Some(mut flags) = ecs.get_mut::<MovementFlags>(entity) {
        *flags = MovementFlags::default();
    }
    if let Some(mut fall) = ecs.get_mut::<FallDistance>(entity) {
        fall.0 = 0.0;
    }
    let health = Health::default();
    if let Some(mut h) = ecs.get_mut::<Health>(entity) {
        *h = health;
    }
    let hunger = Hunger::default();
    if let Some(mut hu) = ecs.get_mut::<Hunger>(entity) {
        *hu = hunger;
    }
    if let Ok(mut e) = ecs.get_entity_mut(entity) {
        e.remove::<Dead>();
        e.remove::<EatingState>();
        e.remove::<MiningState>();
    }
    if let Some(mut container) = ecs.get_mut::<PlayerContainer>(entity) {
        container.open = None;
    }
    if let Some(mut view) = ecs.get_mut::<PlayerViewState>(entity) {
        view.tracked_chunks.clear();
        view.pending_teleport = true;
    }
    let game_mode = ecs
        .get::<PlayerGameMode>(entity)
        .expect("player game mode")
        .0;
    let pose = entity_pose(Position(spawn), Rotation::default(), MovementFlags::default());
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
        health: health.current,
        food: hunger.food,
        saturation: hunger.saturation,
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
    if let Some(mut pos) = ecs.get_mut::<Position>(entity) {
        pos.0 = position;
    } else {
        return;
    }
    let (final_yaw, final_pitch) = {
        let Some(mut rot) = ecs.get_mut::<Rotation>(entity) else {
            return;
        };
        if let Some(y) = yaw {
            rot.yaw = y;
        }
        if let Some(pi) = pitch {
            rot.pitch = pi;
        }
        (rot.yaw, rot.pitch)
    };
    if let Some(mut flags) = ecs.get_mut::<MovementFlags>(entity) {
        flags.on_ground = false;
    }
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
    if let Some(mut gm) = ecs.get_mut::<PlayerGameMode>(entity) {
        gm.0 = mode;
    } else {
        return;
    }
    let health = ecs.get::<Health>(entity).map_or(20.0, |h| h.current);
    let (food, saturation) = ecs
        .get::<Hunger>(entity)
        .map_or((20, 5.0), |h| (h.food, h.saturation));
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
