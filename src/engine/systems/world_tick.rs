//! 20 TPS `bevy_ecs` simulation systems: world clock/weather, player eating &
//! food/vitals, item drop physics/merging/pickup, furnaces, and block/fluid physics.
use crate::{
    engine::{
        components::{
            Dead, DropLifecycle, DropSyncState, EatingState, EntityUuid, Health, Hunger,
            ItemDropMarker, ItemStackComp, NetworkEntityId, PlayerContainer, PlayerGameMode,
            PlayerInventory, PlayerMarker, Position, Velocity, tick_hunger_and_regen,
        },
        io::{ContainerKind, GameOutput},
        resources::{ChestViewers, EntityRegistry, OutputQueue, TickClock, VoxelWorld},
    },
    world::{self, BlockPos},
};
use bevy_ecs::prelude::{Commands, Entity, Query, Res, ResMut, With, Without};

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

/// Advance active item eating only for living players currently carrying `EatingState`.
pub(crate) fn tick_player_eating_system(
    clock: Res<TickClock>,
    mut eaters: Query<
        (
            Entity,
            &EntityUuid,
            &PlayerGameMode,
            &EatingState,
            &Health,
            &mut Hunger,
            &mut PlayerInventory,
        ),
        (With<PlayerMarker>, Without<Dead>),
    >,
    mut commands: Commands,
    mut out: ResMut<OutputQueue>,
) {
    let tick = clock.tick;
    for (entity, uuid, mode, eating, health, mut hunger, mut inv) in &mut eaters {
        if tick.saturating_sub(eating.start_tick) < u64::from(eating.required_ticks) {
            continue;
        }
        commands.entity(entity).remove::<EatingState>();
        if !mode.is_survival() {
            continue;
        }
        let Some(stack) = inv.0.hand(eating.hand) else {
            continue;
        };
        if stack.item != eating.item {
            continue;
        }
        let Some(props) = stack.definition().components.food else {
            continue;
        };
        if !props.always_edible && hunger.food >= 20 {
            continue;
        }

        hunger.food = (hunger.food + props.nutrition).min(20);
        hunger.saturation = (hunger.saturation + props.saturation).min(hunger.food as f32);
        let _ = inv.0.drop_hand(eating.hand, false);
        if let Some(remainder) = crate::game::container_remainder(eating.item) {
            inv.0.give_stack(remainder);
        }

        out.push(GameOutput::PlayerVitalsUpdated {
            uuid: uuid.0,
            health: health.current,
            food: hunger.food,
            saturation: hunger.saturation,
        });
        out.push(GameOutput::PlayerInventorySynced {
            uuid: uuid.0,
            inventory: inv.0.clone(),
        });
    }
}

/// Advance vanilla `FoodData` (exhaustion, regen, starvation) for living players (20 TPS).
pub(crate) fn tick_player_food_system(
    mut players: Query<
        (
            &EntityUuid,
            &NetworkEntityId,
            &PlayerGameMode,
            &mut Health,
            &mut Hunger,
        ),
        (With<PlayerMarker>, Without<Dead>),
    >,
    mut out: ResMut<OutputQueue>,
) {
    for (uuid, net_id, mode, mut health, mut hunger) in &mut players {
        if !mode.is_survival() {
            continue;
        }
        let (vitals_changed, starved) = tick_hunger_and_regen(&mut health, &mut hunger);
        if starved {
            out.push(GameOutput::PlayerHurt {
                uuid: uuid.0,
                entity_id: net_id.0,
            });
        }
        if vitals_changed {
            out.push(GameOutput::PlayerVitalsUpdated {
                uuid: uuid.0,
                health: health.current,
                food: hunger.food,
                saturation: hunger.saturation,
            });
        }
    }
}

/// Advance dropped item physics, collision, periodic sync, expiration, stack merging,
/// and direct proximity pickup into ECS player inventories (20 TPS).
pub(crate) fn tick_item_drops_system(
    world: Res<VoxelWorld>,
    mut registry: ResMut<EntityRegistry>,
    mut drops: Query<
        (
            Entity,
            &NetworkEntityId,
            &mut Position,
            &mut Velocity,
            &mut ItemStackComp,
            &mut DropLifecycle,
            &mut DropSyncState,
        ),
        (With<ItemDropMarker>, Without<PlayerMarker>),
    >,
    mut players: Query<
        (
            &EntityUuid,
            &NetworkEntityId,
            &PlayerGameMode,
            &Position,
            &mut PlayerInventory,
        ),
        (With<PlayerMarker>, Without<Dead>, Without<ItemDropMarker>),
    >,
    mut commands: Commands,
    mut out: ResMut<OutputQueue>,
) {
    let mut removed_entities: Vec<Entity> = Vec::new();
    let mut removed_ids: Vec<i32> = Vec::new();
    let mut active: Vec<(Entity, i32)> = Vec::new();

    // 1. Physics, expiration, sync, and proximity pickup.
    for (entity, net_id, mut pos, mut vel, mut stack, mut life, mut sync) in &mut drops {
        life.age = life.age.saturating_add(1);
        if life.age >= 6000 || pos.0[1] < f64::from(world::MIN_Y) - 64.0 {
            removed_entities.push(entity);
            removed_ids.push(net_id.0);
            continue;
        }
        life.pickup_delay = life.pickup_delay.saturating_sub(1);

        // Skip drop physics when its current chunk is not loaded in memory so
        // item entities never stall the game loop on disk I/O or chunk generation.
        let cur_pos = BlockPos([
            pos.0[0].floor() as i32,
            pos.0[1].floor() as i32,
            pos.0[2].floor() as i32,
        ]);
        if cur_pos.valid() && world.0.block_at_if_loaded(cur_pos).is_none() {
            active.push((entity, net_id.0));
            continue;
        }

        // Vanilla ItemEntity gravity (0.04/tick) and drag.
        vel.0[1] = (vel.0[1] - 0.04).max(-1.0);
        for axis in [0usize, 2usize] {
            let next = pos.0[axis] + vel.0[axis];
            let mut test = pos.0;
            test[axis] = next;
            let bp = BlockPos([
                test[0].floor() as i32,
                test[1].floor() as i32,
                test[2].floor() as i32,
            ]);
            let blocked = bp.valid()
                && world.0.block_at_if_loaded(bp).map_or(true, |s| {
                    crate::game::block_by_state(s).is_some_and(|d| d.collision)
                });
            if blocked {
                vel.0[axis] = 0.0;
            } else {
                pos.0[axis] = next;
            }
        }
        pos.0[1] += vel.0[1];

        let below = BlockPos([
            pos.0[0].floor() as i32,
            (pos.0[1] - 0.125).floor() as i32,
            pos.0[2].floor() as i32,
        ]);
        let on_ground = below.valid()
            && world.0.block_at_if_loaded(below).map_or(true, |s| {
                crate::game::block_by_state(s).is_some_and(|d| d.collision)
            });
        if on_ground {
            pos.0[1] = f64::from(below.0[1] + 1) + 0.125;
            let ground_drag = 0.6 * 0.98;
            vel.0[0] *= ground_drag;
            vel.0[2] *= ground_drag;
            if vel.0[1] < 0.0 {
                vel.0[1] *= -0.5;
                if vel.0[1].abs() < 0.05 {
                    vel.0[1] = 0.0;
                }
            }
        } else {
            vel.0[0] *= 0.98;
            vel.0[1] *= 0.98;
            vel.0[2] *= 0.98;
        }

        // Periodic position/velocity sync.
        sync.ticks_since_sync = sync.ticks_since_sync.saturating_add(1);
        let dx = pos.0[0] - sync.last_synced_pos[0];
        let dy = pos.0[1] - sync.last_synced_pos[1];
        let dz = pos.0[2] - sync.last_synced_pos[2];
        let moved_sq = dx * dx + dy * dy + dz * dz;
        if moved_sq > 0.04 || (sync.ticks_since_sync >= 20 && moved_sq > 1e-4) {
            sync.last_synced_pos = pos.0;
            sync.ticks_since_sync = 0;
            out.push(GameOutput::ItemDropMoved {
                id: net_id.0,
                position: pos.0,
                velocity: vel.0,
            });
        }

        // Direct proximity pickup into ECS player inventory!
        let mut fully_picked = false;
        if life.pickup_delay == 0 {
            for (puuid, pid, pmode, ppos, mut pinv) in &mut players {
                if !pmode.can_interact() {
                    continue;
                }
                let dx = ppos.0[0] - pos.0[0];
                let dy = (ppos.0[1] + 0.9) - pos.0[1];
                let dz = ppos.0[2] - pos.0[2];
                if dx * dx + dy * dy + dz * dz <= 2.25 {
                    let added = pinv.0.give_stack(stack.0);
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
                        if added >= stack.0.count {
                            removed_entities.push(entity);
                            removed_ids.push(net_id.0);
                            fully_picked = true;
                        } else {
                            stack.0.count -= added;
                            out.push(GameOutput::ItemDropUpdated {
                                id: net_id.0,
                                stack: stack.0,
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
            let Ok((_, _, pos, vel, stack, life, _)) = drops.get(e_i) else {
                continue;
            };
            let moving = vel.0[0].abs() + vel.0[2].abs() > 0.003;
            (
                moving,
                life.age,
                stack.0.definition().max_stack,
                stack.0,
                pos.0,
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
            let Ok([
                (_, _, _, _, mut si, mut li, _),
                (_, _, pj, _, mut sj, lj, _),
            ]) = drops.get_many_mut([e_i, e_j])
            else {
                continue;
            };
            let same = si.0.item == sj.0.item && si.0.damage == sj.0.damage;
            if same {
                let dx = pos_i[0] - pj.0[0];
                let dy = pos_i[1] - pj.0[1];
                let dz = pos_i[2] - pj.0[2];
                if dx * dx + dy * dy + dz * dz <= 0.25 {
                    let space = max_stack - si.0.count;
                    let take = space.min(sj.0.count);
                    si.0.count += take;
                    cur_count = si.0.count;
                    li.age = li.age.min(lj.age);
                    li.pickup_delay = li.pickup_delay.max(lj.pickup_delay);
                    out.push(GameOutput::ItemDropUpdated {
                        id: id_i,
                        stack: si.0,
                    });
                    if take == sj.0.count {
                        removed_entities.push(e_j);
                        removed_ids.push(id_j);
                        merged[j] = true;
                    } else {
                        sj.0.count -= take;
                        out.push(GameOutput::ItemDropUpdated {
                            id: id_j,
                            stack: sj.0,
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
        registry.drops.retain(|e| !removed_entities.contains(e));
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
