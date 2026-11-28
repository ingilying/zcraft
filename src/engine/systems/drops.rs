//! Shared ECS helpers for spawning dropped item entities.
use crate::{
    engine::{
        components::{DropState, EntityUuid, ItemDropMarker, NetworkEntityId},
        io::GameOutput,
        resources::{EntityRegistry, MAX_ITEM_DROPS, OutputQueue},
    },
    game::ItemStack,
};
use bevy_ecs::prelude::World;
use rand::Rng;
use uuid::Uuid;

/// Spawn a dropped item entity with vanilla random horizontal scatter.
pub(crate) fn spawn_drop_scatter(
    ecs: &mut World,
    position: [f64; 3],
    stack: ItemStack,
    pickup_delay: u16,
) {
    let mut rng = rand::thread_rng();
    let vx = rng.r#gen::<f64>() * 0.2 - 0.1;
    let vz = rng.r#gen::<f64>() * 0.2 - 0.1;
    spawn_drop(ecs, position, [vx, 0.2, vz], stack, pickup_delay);
}

/// Spawn a dropped item entity in the ECS world and emit sync events.
pub(crate) fn spawn_drop(
    ecs: &mut World,
    position: [f64; 3],
    velocity: [f64; 3],
    stack: ItemStack,
    pickup_delay: u16,
) {
    let Some(id) = ecs.resource::<EntityRegistry>().allocate_entity_id() else {
        return;
    };
    let uuid = Uuid::from_u128(rand::random());

    // Evict oldest active drop if at capacity.
    let mut evicted_id = None;
    loop {
        let at_cap = ecs.resource::<EntityRegistry>().drops.len() >= MAX_ITEM_DROPS;
        if !at_cap {
            break;
        }
        let Some(old_entity) = ecs.resource_mut::<EntityRegistry>().drops.pop_front() else {
            break;
        };
        if let Some(old_net_id) = ecs.get::<NetworkEntityId>(old_entity).copied() {
            evicted_id = Some(old_net_id.0);
            ecs.despawn(old_entity);
            break;
        }
    }

    let entity = ecs
        .spawn((
            ItemDropMarker,
            NetworkEntityId(id),
            EntityUuid(uuid),
            DropState {
                position,
                last_synced_pos: position,
                velocity,
                stack,
                pickup_delay,
                age: 0,
                ticks_since_sync: 0,
            },
        ))
        .id();
    ecs.resource_mut::<EntityRegistry>().drops.push_back(entity);

    let mut out = ecs.resource_mut::<OutputQueue>();
    if let Some(old_id) = evicted_id {
        out.push(GameOutput::EntitiesRemoved(vec![old_id]));
    }
    out.push(GameOutput::ItemDropSpawned {
        id,
        uuid,
        position,
        velocity,
        stack,
    });
}
