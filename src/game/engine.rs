//! `GameEngine`: self-contained `bevy_ecs` World + ordered `GamePhase` Schedule
//! with zero network IO.
use super::{
    components::{
        EntityUuid, ItemDropMarker, ItemStackComp, MovementState, NetworkEntityId,
        PlayerInventory, PlayerProfile, Position, Rotation, Velocity, entity_pose,
    },
    events::{init_messages, update_messages},
    io::{DropSnapshot, GameInput, GameOutput, PlayerSnapshot},
    resources::{ChestViewers, EntityRegistry, InputQueue, OutputQueue, TickClock, VoxelWorld},
    systems::{
        GamePhase,
        drops::spawn_item_drops_system,
        ingress_inputs_system,
        interaction::player_interaction_system,
        lifecycle::{
            player_control_system, player_join_system, player_leave_system, player_view_system,
            world_control_system,
        },
        movement::player_movement_system,
        world_tick::{
            drop_age_and_despawn_system, drop_merge_system, drop_network_sync_system,
            drop_physics_system, drop_pickup_system, tick_block_physics_system,
            tick_furnaces_system, tick_player_eating_system, tick_player_food_system,
            tick_world_clock_system,
        },
    },
};
use crate::{game::Inventory, world::World as VoxelTerrain};
use bevy_ecs::prelude::{IntoScheduleConfigs, Schedule, With, World};
use std::sync::{Arc, atomic::AtomicI32};
use uuid::Uuid;

fn add_input_phases(schedule: &mut Schedule) {
    schedule.add_systems(ingress_inputs_system.in_set(GamePhase::InputIngress));
    schedule.add_systems(
        (player_join_system, player_view_system)
            .chain()
            .in_set(GamePhase::PlayerJoin),
    );
    schedule.add_systems(
        (
            player_control_system,
            world_control_system,
            player_movement_system,
            player_interaction_system,
            player_leave_system,
        )
            .chain()
            .in_set(GamePhase::GameplayActions),
    );
    schedule.add_systems(spawn_item_drops_system.in_set(GamePhase::DropSpawn));
}

/// Authoritative game simulation engine powered by `bevy_ecs`.
///
/// Owns all player entities, item drop entities, container viewers, and runs a
/// deterministic fixed-timestep 20 TPS ECS schedule organized into ordered
/// `GamePhase` `SystemSet`s. Has zero dependency on sockets, sessions, or
/// protocol framing.
pub struct GameEngine {
    ecs: World,
    tick_schedule: Schedule,
    drain_schedule: Schedule,
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
        init_messages(&mut ecs);

        let mut tick_schedule = Schedule::default();
        tick_schedule.configure_sets(
            (
                GamePhase::WorldClock,
                GamePhase::InputIngress,
                GamePhase::PlayerJoin,
                GamePhase::GameplayActions,
                GamePhase::DropSpawn,
                GamePhase::WorldSimulation,
                GamePhase::DropSimulation,
            )
                .chain(),
        );
        tick_schedule.add_systems(tick_world_clock_system.in_set(GamePhase::WorldClock));
        add_input_phases(&mut tick_schedule);
        tick_schedule.add_systems(
            (
                tick_player_eating_system,
                tick_player_food_system,
                tick_furnaces_system,
                tick_block_physics_system,
            )
                .chain()
                .in_set(GamePhase::WorldSimulation),
        );
        tick_schedule.add_systems(
            (
                drop_age_and_despawn_system,
                drop_physics_system,
                drop_pickup_system,
                drop_merge_system,
                drop_network_sync_system,
            )
                .chain()
                .in_set(GamePhase::DropSimulation),
        );

        let mut drain_schedule = Schedule::default();
        drain_schedule.configure_sets(
            (
                GamePhase::InputIngress,
                GamePhase::PlayerJoin,
                GamePhase::GameplayActions,
                GamePhase::DropSpawn,
            )
                .chain(),
        );
        add_input_phases(&mut drain_schedule);

        Self {
            ecs,
            tick_schedule,
            drain_schedule,
        }
    }

    /// Queue an inbound action for the next authoritative `tick()` call.
    pub fn push_input(&mut self, input: GameInput) {
        self.ecs.resource_mut::<InputQueue>().0.push(input);
    }

    /// Drain queued inputs without advancing world simulation ticks.
    /// Used only for headless (no game-loop thread) calls and graceful shutdown drain.
    pub(crate) fn drain_inputs_only(&mut self) -> Vec<GameOutput> {
        if !self.ecs.resource::<InputQueue>().0.is_empty() {
            self.drain_schedule.run(&mut self.ecs);
            update_messages(&mut self.ecs);
        }
        self.ecs.resource_mut::<OutputQueue>().drain()
    }

    /// Advance the entire game simulation by one authoritative 20 TPS tick:
    /// increments `TickClock`, runs the ordered `GamePhase` ECS schedule, and
    /// drains emitted outbound sync events.
    pub fn tick(&mut self) -> Vec<GameOutput> {
        {
            let mut clock = self.ecs.resource_mut::<TickClock>();
            clock.tick = clock.tick.wrapping_add(1);
        }
        self.tick_schedule.run(&mut self.ecs);
        update_messages(&mut self.ecs);
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
        let profile = self.ecs.get::<PlayerProfile>(entity)?;
        let pos = *self.ecs.get::<Position>(entity)?;
        let rot = *self.ecs.get::<Rotation>(entity)?;
        let movement = *self.ecs.get::<MovementState>(entity)?;
        Some(PlayerSnapshot {
            entity_id,
            uuid,
            name: profile.name.clone(),
            game_mode: profile.game_mode,
            position: pos.0,
            yaw: rot.yaw,
            pitch: rot.pitch,
            on_ground: movement.on_ground,
            pose: entity_pose(pos, rot, movement),
            respawn_point: profile.respawn_point,
        })
    }

    /// Clone a player's current authoritative inventory (e.g. across Play reconfiguration).
    pub fn player_inventory(&self, uuid: Uuid) -> Option<Inventory> {
        let &entity = self.ecs.resource::<EntityRegistry>().players.get(&uuid)?;
        self.ecs.get::<PlayerInventory>(entity).map(|i| i.0.clone())
    }
}
