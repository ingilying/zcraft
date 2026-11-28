//! 20 TPS `bevy_ecs` simulation systems: world clock/weather, player eating &
//! food/vitals, furnaces, block/fluid physics, and a 5-stage dropped-item pipeline
//! (age/despawn → physics → pickup → stack merge → throttled network sync).
use crate::{
    game::{
        components::{
            Dead, DropLifecycle, DropSyncState, EatingState, EntityUuid, Health, Hunger,
            ItemDropMarker, ItemStackComp, NetworkEntityId, PlayerContainer, PlayerInventory,
            PlayerMarker, PlayerProfile, Position, Velocity, tick_hunger_and_regen,
        },
        io::{ContainerKind, GameOutput},
        resources::{ChestViewers, EntityRegistry, OutputQueue, TickClock, VoxelWorld},
    },
    world::{self, BlockPos},
};
use bevy_ecs::{
    prelude::{Commands, Entity, Query, Res, ResMut, With, Without},
    query::{QueryData, QueryFilter},
};

#[derive(QueryFilter)]
pub(crate) struct LivingPlayer {
    _player: With<PlayerMarker>,
    _alive: Without<Dead>,
}

#[derive(QueryFilter)]
pub(crate) struct LivingPlayerNotDrop {
    _player: With<PlayerMarker>,
    _alive: Without<Dead>,
    _not_drop: Without<ItemDropMarker>,
}

#[derive(QueryFilter)]
pub(crate) struct ItemDropNotPlayer {
    _drop: With<ItemDropMarker>,
    _not_player: Without<PlayerMarker>,
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(crate) struct EatingPlayer {
    entity: Entity,
    uuid: &'static EntityUuid,
    profile: &'static PlayerProfile,
    eating: &'static EatingState,
    health: &'static Health,
    hunger: &'static mut Hunger,
    inv: &'static mut PlayerInventory,
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(crate) struct FoodPlayer {
    uuid: &'static EntityUuid,
    net_id: &'static NetworkEntityId,
    profile: &'static PlayerProfile,
    health: &'static mut Health,
    hunger: &'static mut Hunger,
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(crate) struct PickupDrop {
    entity: Entity,
    net_id: &'static NetworkEntityId,
    pos: &'static Position,
    life: &'static DropLifecycle,
    stack: &'static mut ItemStackComp,
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(crate) struct PickupPlayer {
    uuid: &'static EntityUuid,
    net_id: &'static NetworkEntityId,
    profile: &'static PlayerProfile,
    pos: &'static Position,
    inv: &'static mut PlayerInventory,
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(crate) struct MergeDrop {
    entity: Entity,
    net_id: &'static NetworkEntityId,
    pos: &'static Position,
    vel: &'static Velocity,
    stack: &'static mut ItemStackComp,
    life: &'static mut DropLifecycle,
}

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
    mut eaters: Query<EatingPlayer, LivingPlayer>,
    mut commands: Commands,
    mut out: ResMut<OutputQueue>,
) {
    let tick = clock.tick;
    for mut p in &mut eaters {
        if tick.saturating_sub(p.eating.start_tick) < u64::from(p.eating.required_ticks) {
            continue;
        }
        commands.entity(p.entity).remove::<EatingState>();
        if !p.profile.is_survival() {
            continue;
        }
        let Some(stack) = p.inv.0.hand(p.eating.hand) else {
            continue;
        };
        if stack.item != p.eating.item {
            continue;
        }
        let Some(props) = stack.definition().components.food else {
            continue;
        };
        if !props.always_edible && p.hunger.food >= 20 {
            continue;
        }

        p.hunger.food = (p.hunger.food + props.nutrition).min(20);
        p.hunger.saturation = (p.hunger.saturation + props.saturation).min(p.hunger.food as f32);
        let _ = p.inv.0.drop_hand(p.eating.hand, false);
        if let Some(remainder) = crate::game::container_remainder(p.eating.item) {
            p.inv.0.give_stack(remainder);
        }

        out.push(GameOutput::PlayerVitalsUpdated {
            uuid: p.uuid.0,
            health: p.health.current,
            food: p.hunger.food,
            saturation: p.hunger.saturation,
        });
        out.push(GameOutput::PlayerInventorySynced {
            uuid: p.uuid.0,
            inventory: p.inv.0.clone(),
        });
    }
}

/// Advance vanilla `FoodData` (exhaustion, regen, starvation) for living players (20 TPS).
pub(crate) fn tick_player_food_system(
    mut players: Query<FoodPlayer, LivingPlayer>,
    mut out: ResMut<OutputQueue>,
) {
    for mut p in &mut players {
        if !p.profile.is_survival() {
            continue;
        }
        let (vitals_changed, starved) = tick_hunger_and_regen(&mut p.health, &mut p.hunger);
        if starved {
            out.push(GameOutput::PlayerHurt {
                uuid: p.uuid.0,
                entity_id: p.net_id.0,
            });
        }
        if vitals_changed {
            out.push(GameOutput::PlayerVitalsUpdated {
                uuid: p.uuid.0,
                health: p.health.current,
                food: p.hunger.food,
                saturation: p.hunger.saturation,
            });
        }
    }
}

/// Drop pipeline stage 1: increment age, tick down pickup delay, and despawn
/// expired (5 min) or void-fallen drops.
pub(crate) fn drop_age_and_despawn_system(
    registry: Res<EntityRegistry>,
    mut drops: Query<
        (Entity, &NetworkEntityId, &Position, &mut DropLifecycle),
        With<ItemDropMarker>,
    >,
    mut commands: Commands,
    mut out: ResMut<OutputQueue>,
) {
    let mut removed_ids = Vec::new();
    for (entity, net_id, pos, mut life) in &mut drops {
        life.age = life.age.saturating_add(1);
        if life.age >= 6000 || pos.0[1] < f64::from(world::MIN_Y) - 64.0 {
            commands.entity(entity).despawn();
            registry.release_entity_id(net_id.0);
            removed_ids.push(net_id.0);
            continue;
        }
        life.pickup_delay = life.pickup_delay.saturating_sub(1);
    }
    if !removed_ids.is_empty() {
        out.push(GameOutput::EntitiesRemoved(removed_ids));
    }
}

/// Drop pipeline stage 2: integrate gravity, air/ground drag, and voxel block collision.
pub(crate) fn drop_physics_system(
    world: Res<VoxelWorld>,
    mut drops: Query<(&mut Position, &mut Velocity), With<ItemDropMarker>>,
) {
    for (mut pos, mut vel) in &mut drops {
        // Skip drop physics when its current chunk is not loaded in memory so
        // item entities never stall the game loop on disk I/O or chunk generation.
        let cur_pos = BlockPos([
            pos.0[0].floor() as i32,
            pos.0[1].floor() as i32,
            pos.0[2].floor() as i32,
        ]);
        if cur_pos.valid() && world.0.block_at_if_loaded(cur_pos).is_none() {
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
                && world
                    .0
                    .block_at_if_loaded(bp)
                    .is_none_or(|s| crate::game::block_by_state(s).is_some_and(|d| d.collision));
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
            && world
                .0
                .block_at_if_loaded(below)
                .is_none_or(|s| crate::game::block_by_state(s).is_some_and(|d| d.collision));
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
    }
}

/// Drop pipeline stage 3: proximity pickup into living players' inventories.
pub(crate) fn drop_pickup_system(
    registry: Res<EntityRegistry>,
    mut drops: Query<PickupDrop, ItemDropNotPlayer>,
    mut players: Query<PickupPlayer, LivingPlayerNotDrop>,
    mut commands: Commands,
    mut out: ResMut<OutputQueue>,
) {
    let mut removed_ids = Vec::new();

    for mut drop in &mut drops {
        if drop.life.pickup_delay != 0 {
            continue;
        }
        for mut player in &mut players {
            if !player.profile.can_interact() {
                continue;
            }
            let dx = player.pos.0[0] - drop.pos.0[0];
            let dy = (player.pos.0[1] + 0.9) - drop.pos.0[1];
            let dz = player.pos.0[2] - drop.pos.0[2];
            if dx * dx + dy * dy + dz * dz <= 2.25 {
                let added = player.inv.0.give_stack(drop.stack.0);
                if added > 0 {
                    out.push(GameOutput::ItemDropCollected {
                        drop_id: drop.net_id.0,
                        collector_id: player.net_id.0,
                        count: added,
                    });
                    out.push(GameOutput::PlayerInventorySynced {
                        uuid: player.uuid.0,
                        inventory: player.inv.0.clone(),
                    });
                    if added >= drop.stack.0.count {
                        commands.entity(drop.entity).despawn();
                        registry.release_entity_id(drop.net_id.0);
                        removed_ids.push(drop.net_id.0);
                    } else {
                        drop.stack.0.count -= added;
                        out.push(GameOutput::ItemDropUpdated {
                            id: drop.net_id.0,
                            stack: drop.stack.0,
                        });
                    }
                    break;
                }
            }
        }
    }

    if !removed_ids.is_empty() {
        out.push(GameOutput::EntitiesRemoved(removed_ids));
    }
}

/// Drop pipeline stage 4: merge nearby identical item drop stacks.
pub(crate) fn drop_merge_system(
    registry: Res<EntityRegistry>,
    mut drops: Query<MergeDrop, With<ItemDropMarker>>,
    mut commands: Commands,
    mut out: ResMut<OutputQueue>,
) {
    let active: Vec<(Entity, i32)> = drops.iter().map(|d| (d.entity, d.net_id.0)).collect();
    if active.len() < 2 {
        return;
    }

    let mut removed_ids = Vec::new();
    let mut merged = vec![false; active.len()];

    for i in 0..active.len() {
        if merged[i] {
            continue;
        }
        let (e_i, id_i) = active[i];
        let (moving, age_i, max_stack, stack_i, pos_i) = {
            let Ok(d) = drops.get(e_i) else {
                continue;
            };
            let moving = d.vel.0[0].abs() + d.vel.0[2].abs() > 0.003;
            (
                moving,
                d.life.age,
                d.stack.0.definition().max_stack,
                d.stack.0,
                d.pos.0,
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
            let Ok([mut di, mut dj]) = drops.get_many_mut([e_i, e_j]) else {
                continue;
            };
            let same = di.stack.0.item == dj.stack.0.item && di.stack.0.damage == dj.stack.0.damage;
            if same {
                let dx = pos_i[0] - dj.pos.0[0];
                let dy = pos_i[1] - dj.pos.0[1];
                let dz = pos_i[2] - dj.pos.0[2];
                if dx * dx + dy * dy + dz * dz <= 0.25 {
                    let space = max_stack - di.stack.0.count;
                    let take = space.min(dj.stack.0.count);
                    di.stack.0.count += take;
                    cur_count = di.stack.0.count;
                    di.life.age = di.life.age.min(dj.life.age);
                    di.life.pickup_delay = di.life.pickup_delay.max(dj.life.pickup_delay);
                    out.push(GameOutput::ItemDropUpdated {
                        id: id_i,
                        stack: di.stack.0,
                    });
                    if take == dj.stack.0.count {
                        commands.entity(e_j).despawn();
                        registry.release_entity_id(id_j);
                        removed_ids.push(id_j);
                        merged[j] = true;
                    } else {
                        dj.stack.0.count -= take;
                        out.push(GameOutput::ItemDropUpdated {
                            id: id_j,
                            stack: dj.stack.0,
                        });
                    }
                }
            }
        }
    }

    if !removed_ids.is_empty() {
        out.push(GameOutput::EntitiesRemoved(removed_ids));
    }
}

/// Drop pipeline stage 5: emit throttled `ItemDropMoved` updates when drops move.
pub(crate) fn drop_network_sync_system(
    mut drops: Query<
        (&NetworkEntityId, &Position, &Velocity, &mut DropSyncState),
        With<ItemDropMarker>,
    >,
    mut out: ResMut<OutputQueue>,
) {
    for (net_id, pos, vel, mut sync) in &mut drops {
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
