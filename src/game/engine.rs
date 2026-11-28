//! `GameEngine`: self-contained `bevy_ecs` World + Schedules with zero network IO.
use super::{
    components::{
        EntityUuid, ItemDropMarker, ItemStackComp, MovementFlags, NetworkEntityId, PlayerGameMode,
        PlayerInventory, PlayerName, Position, RespawnPoint, Rotation, Velocity, entity_pose,
    },
    io::{DropSnapshot, GameInput, GameOutput, PlayerSnapshot},
    resources::{ChestViewers, EntityRegistry, InputQueue, OutputQueue, TickClock, VoxelWorld},
    systems::{
        process_inputs_system,
        world_tick::{
            tick_block_physics_system, tick_furnaces_system, tick_item_drops_system,
            tick_player_eating_system, tick_player_food_system, tick_world_clock_system,
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
                tick_player_eating_system,
                tick_player_food_system,
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
        let mut query = self.ecs.query_filtered::<(
            &NetworkEntityId,
            &EntityUuid,
            &Position,
            &Velocity,
            &ItemStackComp,
        ), With<ItemDropMarker>>();
        query
            .iter(&self.ecs)
            .map(|(id, uuid, pos, vel, stack)| DropSnapshot {
                id: id.0,
                uuid: uuid.0,
                position: pos.0,
                velocity: vel.0,
                stack: stack.0,
            })
            .collect()
    }

    /// Read-only snapshot of an online player entity.
    pub fn player_snapshot(&self, uuid: Uuid) -> Option<PlayerSnapshot> {
        let &entity = self.ecs.resource::<EntityRegistry>().players.get(&uuid)?;
        let entity_id = self.ecs.get::<NetworkEntityId>(entity)?.0;
        let name = self.ecs.get::<PlayerName>(entity)?.0.clone();
        let game_mode = self.ecs.get::<PlayerGameMode>(entity)?.0;
        let pos = *self.ecs.get::<Position>(entity)?;
        let rot = *self.ecs.get::<Rotation>(entity)?;
        let flags = *self.ecs.get::<MovementFlags>(entity)?;
        let respawn_point = self.ecs.get::<RespawnPoint>(entity)?.0;
        Some(PlayerSnapshot {
            entity_id,
            uuid,
            name,
            game_mode,
            position: pos.0,
            yaw: rot.yaw,
            pitch: rot.pitch,
            on_ground: flags.on_ground,
            pose: entity_pose(pos, rot, flags),
            respawn_point,
        })
    }

    /// Clone a player's current authoritative inventory (e.g. across Play reconfiguration).
    pub fn player_inventory(&self, uuid: Uuid) -> Option<Inventory> {
        let &entity = self.ecs.resource::<EntityRegistry>().players.get(&uuid)?;
        self.ecs.get::<PlayerInventory>(entity).map(|i| i.0.clone())
    }
}
