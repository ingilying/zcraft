//! 20 TPS `bevy_ecs` simulation systems: world clock/weather, player vitals/eating,
//! item drop physics/merging/pickup, furnaces, and block/fluid physics.
use crate::{
    engine::{
        components::{
            DropState, EntityUuid, ItemDropMarker, NetworkEntityId, PlayerContainer, PlayerData,
            PlayerInventory, PlayerMarker,
        },
        io::{ContainerKind, GameOutput},
        resources::{ChestViewers, EntityRegistry, OutputQueue, TickClock, VoxelWorld},
    },
    world::{self, BlockPos},
};
use bevy_ecs::prelude::{Commands, Entity, Query, Res, ResMut, With};

/// Advance world clock and weather cycle (20 TPS).
pub(crate) fn tick_world_clock_system(world: Res<VoxelWorld>, mut out: ResMut<OutputQueue>) {
    let events = world.0.tick();
    if let Some(time) = events.time_sync {
        out.push(GameOutput::TimeUpdated(time));
    }
    for weather in events.weather {
        out.push(GameOutput::WeatherChanged(weather));
    }
}

/// Advance continuous item eating and vanilla `FoodData` (exhaustion, regen, starvation) at 20 TPS.
pub(crate) fn tick_player_vitals_system(
    clock: Res<TickClock>,
    mut players: Query<(&EntityUuid, &mut PlayerData, &mut PlayerInventory), With<PlayerMarker>>,
    mut out: ResMut<OutputQueue>,
) {
    let tick = clock.tick;
    for (uuid, mut player_data, mut inv) in &mut players {
        let player = &mut player_data.0;
        let mut inv_changed = false;
        let mut health_changed = false;

        if let Some((hand, item_id, start_tick, eat_ticks)) = player.eating {
            if tick.saturating_sub(start_tick) >= u64::from(eat_ticks) {
                player.eating = None;
                if let Some(stack) = inv.0.hand(hand)
                    && stack.item == item_id
                    && crate::game::item_behavior(item_id).on_finish_use(player, stack)
                {
                    let _ = inv.0.drop_hand(hand, false);
                    if let Some(remainder) = crate::game::container_remainder(item_id) {
                        inv.0.give_stack(remainder);
                    }
                    inv_changed = true;
                    health_changed = true;
                }
            }
        }

        let (food_changed, starved) = player.tick_food(1);
        health_changed |= food_changed;
        if starved {
            out.push(GameOutput::PlayerHurt {
                uuid: uuid.0,
                entity_id: player.entity_id,
            });
        }
        if health_changed {
            out.push(GameOutput::PlayerVitalsUpdated {
                uuid: uuid.0,
                health: player.health,
                food: player.food,
                saturation: player.saturation,
            });
        }
        if inv_changed {
            out.push(GameOutput::PlayerInventorySynced {
                uuid: uuid.0,
                inventory: inv.0.clone(),
            });
        }
    }
}

/// Advance dropped item physics, collision, periodic sync, expiration, stack merging,
/// and direct proximity pickup into ECS player inventories (20 TPS).
pub(crate) fn tick_item_drops_system(
    world: Res<VoxelWorld>,
    mut registry: ResMut<EntityRegistry>,
    mut drops: Query<(Entity, &NetworkEntityId, &mut DropState), With<ItemDropMarker>>,
    mut players: Query<
        (&EntityUuid, &NetworkEntityId, &PlayerData, &mut PlayerInventory),
        With<PlayerMarker>,
    >,
    mut commands: Commands,
    mut out: ResMut<OutputQueue>,
) {
    let mut removed_entities: Vec<Entity> = Vec::new();
    let mut removed_ids: Vec<i32> = Vec::new();
    let mut active: Vec<(Entity, i32)> = Vec::new();

    // 1. Physics, expiration, sync, and proximity pickup.
    for (entity, net_id, mut drop) in &mut drops {
        drop.age = drop.age.saturating_add(1);
        if drop.age >= 6000 || drop.position[1] < f64::from(world::MIN_Y) - 64.0 {
            removed_entities.push(entity);
            removed_ids.push(net_id.0);
            continue;
        }
        drop.pickup_delay = drop.pickup_delay.saturating_sub(1);

        // Skip drop physics when its current chunk is not loaded in memory so
        // item entities never stall the game loop on disk I/O or chunk generation.
        let cur_pos = BlockPos([
            drop.position[0].floor() as i32,
            drop.position[1].floor() as i32,
            drop.position[2].floor() as i32,
        ]);
        if cur_pos.valid() && world.0.block_at_if_loaded(cur_pos).is_none() {
            active.push((entity, net_id.0));
            continue;
        }

        // Vanilla ItemEntity gravity (0.04/tick) and drag.
        drop.velocity[1] = (drop.velocity[1] - 0.04).max(-1.0);
        for axis in [0usize, 2usize] {
            let next = drop.position[axis] + drop.velocity[axis];
            let mut test = drop.position;
            test[axis] = next;
            let bp = BlockPos([
                test[0].floor() as i32,
                test[1].floor() as i32,
                test[2].floor() as i32,
            ]);
            let blocked = bp.valid()
                && world
                    .0
                    .block_at_if_loaded(bp)
                    .map_or(true, |s| crate::game::block_by_state(s).is_some_and(|d| d.collision));
            if blocked {
                drop.velocity[axis] = 0.0;
            } else {
                drop.position[axis] = next;
            }
        }
        drop.position[1] += drop.velocity[1];

        let below = BlockPos([
            drop.position[0].floor() as i32,
            (drop.position[1] - 0.125).floor() as i32,
            drop.position[2].floor() as i32,
        ]);
        let on_ground = below.valid()
            && world
                .0
                .block_at_if_loaded(below)
                .map_or(true, |s| crate::game::block_by_state(s).is_some_and(|d| d.collision));
        if on_ground {
            drop.position[1] = f64::from(below.0[1] + 1) + 0.125;
            let ground_drag = 0.6 * 0.98;
            drop.velocity[0] *= ground_drag;
            drop.velocity[2] *= ground_drag;
            if drop.velocity[1] < 0.0 {
                drop.velocity[1] *= -0.5;
                if drop.velocity[1].abs() < 0.05 {
                    drop.velocity[1] = 0.0;
                }
            }
        } else {
            drop.velocity[0] *= 0.98;
            drop.velocity[1] *= 0.98;
            drop.velocity[2] *= 0.98;
        }

        // Periodic position/velocity sync.
        drop.ticks_since_sync = drop.ticks_since_sync.saturating_add(1);
        let dx = drop.position[0] - drop.last_synced_pos[0];
        let dy = drop.position[1] - drop.last_synced_pos[1];
        let dz = drop.position[2] - drop.last_synced_pos[2];
        let moved_sq = dx * dx + dy * dy + dz * dz;
        if moved_sq > 0.04 || (drop.ticks_since_sync >= 20 && moved_sq > 1e-4) {
            drop.last_synced_pos = drop.position;
            drop.ticks_since_sync = 0;
            out.push(GameOutput::ItemDropMoved {
                id: net_id.0,
                position: drop.position,
                velocity: drop.velocity,
            });
        }

        // Direct proximity pickup into ECS player inventory!
        let mut fully_picked = false;
        if drop.pickup_delay == 0 {
            for (puuid, pid, pdata, mut pinv) in &mut players {
                if pdata.0.dead || !pdata.0.can_interact() {
                    continue;
                }
                let ppos = pdata.0.position;
                let dx = ppos[0] - drop.position[0];
                let dy = (ppos[1] + 0.9) - drop.position[1];
                let dz = ppos[2] - drop.position[2];
                if dx * dx + dy * dy + dz * dz <= 2.25 {
                    let added = pinv.0.give_stack(drop.stack);
                    if added > 0 {
                        out.push(GameOutput::ItemDropCollected {
                            drop_id: net_id.0,
                            collector_id: pid.0,
                            count: added,
                        });
                        out.push(GameOutput::PlayerInventorySynced {
                            uuid: puuid.0,
                            inventory: pinv.0.clone(),
                        });
                        if added >= drop.stack.count {
                            removed_entities.push(entity);
                            removed_ids.push(net_id.0);
                            fully_picked = true;
                        } else {
                            drop.stack.count -= added;
                            out.push(GameOutput::ItemDropUpdated {
                                id: net_id.0,
                                stack: drop.stack,
                            });
                        }
                        break;
                    }
                }
            }
        }
        if !fully_picked {
            active.push((entity, net_id.0));
        }
    }

    // 2. Merge nearby identical stacks (`ItemEntity.mergeWithNeighbours`).
    let mut merged = vec![false; active.len()];
    for i in 0..active.len() {
        if merged[i] {
            continue;
        }
        let (e_i, id_i) = active[i];
        let (moving, age_i, max_stack, stack_i, pos_i) = {
            let Ok((_, _, d)) = drops.get(e_i) else {
                continue;
            };
            let moving = d.velocity[0].abs() + d.velocity[2].abs() > 0.003;
            (
                moving,
                d.age,
                d.stack.definition().max_stack,
                d.stack,
                d.position,
            )
        };
        let interval = if moving { 2 } else { 40 };
        if !age_i.is_multiple_of(interval) || stack_i.count >= max_stack {
            continue;
        }
        let mut cur_count = stack_i.count;
        for j in (i + 1)..active.len() {
            if cur_count >= max_stack {
                break;
            }
            if merged[j] {
                continue;
            }
            let (e_j, id_j) = active[j];
            let Ok([(_, _, mut di), (_, _, mut dj)]) = drops.get_many_mut([e_i, e_j]) else {
                continue;
            };
            let same = di.stack.item == dj.stack.item && di.stack.damage == dj.stack.damage;
            if same {
                let dx = pos_i[0] - dj.position[0];
                let dy = pos_i[1] - dj.position[1];
                let dz = pos_i[2] - dj.position[2];
                if dx * dx + dy * dy + dz * dz <= 0.25 {
                    let space = max_stack - di.stack.count;
                    let take = space.min(dj.stack.count);
                    di.stack.count += take;
                    cur_count = di.stack.count;
                    di.age = di.age.min(dj.age);
                    di.pickup_delay = di.pickup_delay.max(dj.pickup_delay);
                    out.push(GameOutput::ItemDropUpdated {
                        id: id_i,
                        stack: di.stack,
                    });
                    if take == dj.stack.count {
                        removed_entities.push(e_j);
                        removed_ids.push(id_j);
                        merged[j] = true;
                    } else {
                        dj.stack.count -= take;
                        out.push(GameOutput::ItemDropUpdated {
                            id: id_j,
                            stack: dj.stack,
                        });
                    }
                }
            }
        }
    }

    if !removed_entities.is_empty() {
        for entity in &removed_entities {
            commands.entity(*entity).despawn();
        }
        registry
            .drops
            .retain(|e| !removed_entities.contains(e));
    }
    if !removed_ids.is_empty() {
        out.push(GameOutput::EntitiesRemoved(removed_ids));
    }
}

/// Advance all loaded furnaces by 1 tick (20 TPS) and refresh any ECS players viewing them.
pub(crate) fn tick_furnaces_system(
    world: Res<VoxelWorld>,
    viewers: Res<ChestViewers>,
    registry: Res<EntityRegistry>,
    mut players: Query<(&mut PlayerContainer, &PlayerInventory), With<PlayerMarker>>,
    mut out: ResMut<OutputQueue>,
) {
    let (changed_positions, mutations) = world.0.tick_furnaces();
    for mutation in mutations {
        out.push(GameOutput::WorldMutated(mutation));
    }
    for pos in changed_positions {
        let Some(furnace) = world.0.furnace_data(pos) else {
            continue;
        };
        let props = [
            furnace.burn_time as i16,
            furnace.burn_total as i16,
            furnace.cook_time as i16,
            furnace.cook_total as i16,
        ];
        for viewer_uuid in viewers.others(pos, uuid::Uuid::nil()) {
            let Some(&entity) = registry.players.get(&viewer_uuid) else {
                continue;
            };
            let Ok((mut container, inv)) = players.get_mut(entity) else {
                continue;
            };
            if let Some(open) = container.open.as_mut()
                && matches!(open.kind, ContainerKind::Furnace(fpos) if fpos == pos)
            {
                open.state_id = open.state_id.wrapping_add(1);
                let slots = crate::game::containers::furnace_slots(&furnace, &inv.0);
                out.push(GameOutput::ContainerSynced {
                    uuid: viewer_uuid,
                    window_id: open.window_id,
                    state_id: open.state_id,
                    slots,
                    carried: inv.0.carried(),
                    furnace_props: Some(props),
                });
            }
        }
    }
}

/// Advance scheduled block & fluid physics by 1 tick (20 TPS).
pub(crate) fn tick_block_physics_system(world: Res<VoxelWorld>, mut out: ResMut<OutputQueue>) {
    for mutation in world.0.tick_physics() {
        out.push(GameOutput::WorldMutated(mutation));
    }
}
