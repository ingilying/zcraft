//! `GameEngine`: self-contained `bevy_ecs` World + Schedules with zero network IO.
use super::{
    components::{
        DropState, EntityUuid, ItemDropMarker, NetworkEntityId, PlayerData, PlayerInventory,
    },
    io::{DropSnapshot, GameInput, GameOutput, PlayerSnapshot},
    resources::{ChestViewers, EntityRegistry, InputQueue, OutputQueue, TickClock, VoxelWorld},
    systems::{
        process_inputs_system,
        world_tick::{
            tick_block_physics_system, tick_furnaces_system, tick_item_drops_system,
            tick_player_vitals_system, tick_world_clock_system,
        },
    },
};
use crate::{game::Inventory, world::World as VoxelTerrain};
use bevy_ecs::prelude::{IntoScheduleConfigs, Schedule, With, World};
use std::sync::{Arc, atomic::AtomicI32};
use uuid::Uuid;

/// Authoritative game simulation engine powered by `bevy_ecs`.
///
/// Owns all player entities, item drop entities, container viewers, and runs a
/// single deterministic fixed-timestep 20 TPS ECS schedule.
/// Has zero dependency on sockets, sessions, or protocol framing.
pub struct GameEngine {
    ecs: World,
    tick_schedule: Schedule,
}

impl GameEngine {
    pub fn new(world: Arc<VoxelTerrain>, next_entity_id: Arc<AtomicI32>) -> Self {
        let mut ecs = World::new();
        ecs.insert_resource(VoxelWorld(world));
        ecs.insert_resource(EntityRegistry::new(next_entity_id));
        ecs.insert_resource(ChestViewers::default());
        ecs.insert_resource(TickClock::default());
        ecs.insert_resource(InputQueue::default());
        ecs.insert_resource(OutputQueue::default());

        let mut tick_schedule = Schedule::default();
        tick_schedule.add_systems(
            (
                tick_world_clock_system,
                process_inputs_system,
                tick_player_vitals_system,
                tick_item_drops_system,
                tick_furnaces_system,
                tick_block_physics_system,
            )
                .chain(),
        );

        Self { ecs, tick_schedule }
    }

    /// Queue an inbound action for the next authoritative `tick()` call.
    pub fn push_input(&mut self, input: GameInput) {
        self.ecs.resource_mut::<InputQueue>().0.push(input);
    }

    /// Drain queued inputs without advancing world simulation ticks.
    /// Used only for headless (no game-loop thread) calls and graceful shutdown drain.
    pub(crate) fn drain_inputs_only(&mut self) -> Vec<GameOutput> {
        if !self.ecs.resource::<InputQueue>().0.is_empty() {
            process_inputs_system(&mut self.ecs);
        }
        self.ecs.resource_mut::<OutputQueue>().drain()
    }

    /// Advance the entire game simulation by one authoritative 20 TPS tick:
    /// increments `TickClock`, then runs the unified fixed-timestep ECS schedule.
    pub fn tick(&mut self) -> Vec<GameOutput> {
        {
            let mut clock = self.ecs.resource_mut::<TickClock>();
            clock.tick = clock.tick.wrapping_add(1);
        }
        self.tick_schedule.run(&mut self.ecs);
        self.ecs.resource_mut::<OutputQueue>().drain()
    }

    /// Current authoritative tick count.
    pub fn tick_count(&self) -> u64 {
        self.ecs.resource::<TickClock>().tick
    }

    /// Snapshot all active item drop entities (for joining/respawning players).
    pub fn existing_drops(&mut self) -> Vec<DropSnapshot> {
        let mut query = self
            .ecs
            .query_filtered::<(&NetworkEntityId, &EntityUuid, &DropState), With<ItemDropMarker>>();
        query
            .iter(&self.ecs)
            .map(|(id, uuid, drop)| DropSnapshot {
                id: id.0,
                uuid: uuid.0,
                position: drop.position,
                velocity: drop.velocity,
                stack: drop.stack,
            })
            .collect()
    }

    /// Read-only snapshot of an online player entity.
    pub fn player_snapshot(&self, uuid: Uuid) -> Option<PlayerSnapshot> {
        let &entity = self.ecs.resource::<EntityRegistry>().players.get(&uuid)?;
        let p = &self.ecs.get::<PlayerData>(entity)?.0;
        Some(PlayerSnapshot {
            entity_id: p.entity_id,
            uuid: p.uuid,
            name: p.name.clone(),
            game_mode: p.game_mode,
            position: p.position,
            yaw: p.yaw,
            pitch: p.pitch,
            on_ground: p.on_ground,
            pose: p.pose(),
            respawn_point: p.respawn_point,
        })
    }

    /// Clone a player's current authoritative inventory (e.g. across Play reconfiguration).
    pub fn player_inventory(&self, uuid: Uuid) -> Option<Inventory> {
        let &entity = self.ecs.resource::<EntityRegistry>().players.get(&uuid)?;
        self.ecs.get::<PlayerInventory>(entity).map(|i| i.0.clone())
    }
}
