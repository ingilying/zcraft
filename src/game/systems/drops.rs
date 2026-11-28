//! Declarative ECS system for spawning dropped item entities from `SpawnDropMsg`.
use crate::game::{
    ItemStack,
    components::{
        DropLifecycle, DropSyncState, EntityUuid, ItemDropMarker, ItemStackComp, NetworkEntityId,
        Position, Velocity,
    },
    events::SpawnDropMsg,
    io::GameOutput,
    resources::{EntityRegistry, MAX_ITEM_DROPS, OutputQueue},
};
use bevy_ecs::prelude::{Commands, MessageReader, Messages, Query, ResMut, World};
use rand::Rng;
use uuid::Uuid;

/// Queue a dropped item spawn request with vanilla random horizontal scatter.
pub(crate) fn queue_drop_scatter(
    ecs: &mut World,
    position: [f64; 3],
    stack: ItemStack,
    pickup_delay: u16,
) {
    ecs.resource_mut::<Messages<SpawnDropMsg>>()
        .write(SpawnDropMsg::scatter(position, stack, pickup_delay));
}

/// Queue a dropped item spawn request with explicit initial velocity.
pub(crate) fn queue_drop(
    ecs: &mut World,
    position: [f64; 3],
    velocity: [f64; 3],
    stack: ItemStack,
    pickup_delay: u16,
) {
    ecs.resource_mut::<Messages<SpawnDropMsg>>()
        .write(SpawnDropMsg::with_velocity(
            position,
            velocity,
            stack,
            pickup_delay,
        ));
}

/// Spawn dropped item entities requested via `SpawnDropMsg`, enforcing `MAX_ITEM_DROPS`.
pub(crate) fn spawn_item_drops_system(
    mut requests: MessageReader<SpawnDropMsg>,
    mut commands: Commands,
    mut registry: ResMut<EntityRegistry>,
    drop_ids: Query<&NetworkEntityId>,
    mut out: ResMut<OutputQueue>,
) {
    let mut rng = rand::thread_rng();
    for req in requests.read() {
        let Some(id) = registry.allocate_entity_id() else {
            continue;
        };
        let uuid = Uuid::from_u128(rand::random());
        let velocity = req.velocity.unwrap_or_else(|| {
            let vx = rng.r#gen::<f64>() * 0.2 - 0.1;
            let vz = rng.r#gen::<f64>() * 0.2 - 0.1;
            [vx, 0.2, vz]
        });

        // Evict oldest active drop if at capacity.
        let mut evicted_id = None;
        while registry.drops.len() >= MAX_ITEM_DROPS {
            let Some(old_entity) = registry.drops.pop_front() else {
                break;
            };
            if let Ok(old_net_id) = drop_ids.get(old_entity) {
                evicted_id = Some(old_net_id.0);
                commands.entity(old_entity).despawn();
                break;
            }
        }

        let entity = commands
            .spawn((
                ItemDropMarker,
                NetworkEntityId(id),
                EntityUuid(uuid),
                Position(req.position),
                Velocity(velocity),
                ItemStackComp(req.stack),
                DropLifecycle {
                    age: 0,
                    pickup_delay: req.pickup_delay,
                },
                DropSyncState {
                    last_synced_pos: req.position,
                    ticks_since_sync: 0,
                },
            ))
            .id();
        registry.drops.push_back(entity);

        if let Some(old_id) = evicted_id {
            out.push(GameOutput::EntitiesRemoved(vec![old_id]));
        }
        out.push(GameOutput::ItemDropSpawned {
            id,
            uuid,
            position: req.position,
            velocity,
            stack: req.stack,
        });
    }
}
