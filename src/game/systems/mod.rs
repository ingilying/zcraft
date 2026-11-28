//! Modular `bevy_ecs` systems organized into ordered `GamePhase` `SystemSet`s.
pub(crate) mod drops;
pub(crate) mod interaction;
pub(crate) mod lifecycle;
pub(crate) mod movement;
pub(crate) mod world_tick;

pub(crate) use super::events::ingress_inputs_system;
use bevy_ecs::prelude::SystemSet;

/// Ordered execution phases for the deterministic 20 TPS ECS schedule.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GamePhase {
    /// Advance world clock and weather cycle.
    WorldClock,
    /// Drain `InputQueue` and dispatch into typed ECS message streams.
    InputIngress,
    /// Spawn joining players and update transport-synced view/teleport state.
    PlayerJoin,
    /// Execute authoritative gameplay controls, movement, interactions, and leaves.
    GameplayActions,
    /// Spawn newly requested item drop entities from `SpawnDropMsg`.
    DropSpawn,
    /// Advance periodic player & world simulation (eating, hunger, furnaces, block physics).
    WorldSimulation,
    /// Advance dropped item lifecycle, physics, pickup, merging, and network sync.
    DropSimulation,
}
