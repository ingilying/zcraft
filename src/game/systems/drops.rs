//! Declarative ECS system for spawning dropped item entities from `SpawnDropMsg`.
use crate::game::{
    components::{
        DropLifecycle, DropSyncState, EntityUuid, ItemDropMarker, ItemStackComp, NetworkEntityId,
        Position, Velocity,
    },
    events::SpawnDropMsg,
    io::GameOutput,
    resources::{EntityRegistry, MAX_ITEM_DROPS, OutputQueue},
};
use bevy_ecs::prelude::{Commands, Entity, MessageReader, Query, Res, ResMut, With};
use rand::Rng;
use uuid::Uuid;

/// Spawn dropped item entities requested via `SpawnDropMsg`, enforcing `MAX_ITEM_DROPS`
/// by evicting the oldest active drops by `DropLifecycle::age` (no manual drop queue).
pub(crate) fn spawn_item_drops_system(
    mut requests: MessageReader<SpawnDropMsg>,
    mut commands: Commands,
    registry: Res<EntityRegistry>,
    existing_drops: Query<(Entity, &NetworkEntityId, &DropLifecycle), With<ItemDropMarker>>,
    mut out: ResMut<OutputQueue>,
) {
    let pending: Vec<SpawnDropMsg> = requests.read().copied().collect();
    if pending.is_empty() {
        return;
    }

    let mut evict_candidates: Option<Vec<(Entity, i32, u16)>> = None;
    let mut live_count = existing_drops.iter().len();
    let mut evicted_ids = Vec::new();
    let mut rng = rand::thread_rng();

    for req in pending {
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
        if live_count >= MAX_ITEM_DROPS {
            let candidates = evict_candidates.get_or_insert_with(|| {
                let mut list: Vec<(Entity, i32, u16)> = existing_drops
                    .iter()
                    .map(|(e, nid, life)| (e, nid.0, life.age))
                    .collect();
                // Sort ascending by age so `.pop()` evicts the oldest drop in O(1).
                list.sort_unstable_by_key(|&(_, _, age)| age);
                list
            });
            if let Some((old_entity, old_net_id, _)) = candidates.pop() {
                commands.entity(old_entity).despawn();
                registry.release_entity_id(old_net_id);
                evicted_ids.push(old_net_id);
                live_count -= 1;
            }
        }

        commands.spawn((
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
        ));
        live_count += 1;

        out.push(GameOutput::ItemDropSpawned {
            id,
            uuid,
            position: req.position,
            velocity,
            stack: req.stack,
        });
    }

    if !evicted_ids.is_empty() {
        out.push(GameOutput::EntitiesRemoved(evicted_ids));
    }
}
