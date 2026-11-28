//! ECS Components for authoritative gameplay entities (Players, ItemDrops).
use super::io::OpenContainer;
use crate::{
    game::{Inventory, ItemStack, Player},
    world::BlockPos,
};
use bevy_ecs::prelude::Component;
use std::collections::HashSet;
use uuid::Uuid;

/// Marker component for online player entities.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct PlayerMarker;

/// Marker component for dropped item entities in the world.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct ItemDropMarker;

/// Protocol-visible entity ID shared by players and world entities.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkEntityId(pub i32);

/// Stable UUID for entities.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityUuid(pub Uuid);

/// Authoritative player gameplay state (pose, vitals, food, gamemode, respawn point).
#[derive(Component, Debug, Clone)]
pub struct PlayerData(pub Player);

/// Authoritative 46-slot player inventory.
#[derive(Component, Debug, Clone)]
pub struct PlayerInventory(pub Inventory);

/// Active Survival-mode block mining state: `(target_pos, start_tick)`.
#[derive(Component, Debug, Default, Clone)]
pub struct PlayerMining(pub Option<(BlockPos, u64)>);

/// Active open container screen state for a player.
#[derive(Component, Debug, Clone)]
pub struct PlayerContainer {
    pub open: Option<OpenContainer>,
    pub next_window_id: u8,
}

impl Default for PlayerContainer {
    fn default() -> Self {
        Self {
            open: None,
            next_window_id: 1,
        }
    }
}

/// Gameplay-relevant view/teleport gating state synced from the transport layer.
#[derive(Component, Debug, Default, Clone)]
pub struct PlayerViewState {
    pub tracked_chunks: HashSet<[i32; 2]>,
    pub pending_teleport: bool,
}

/// Authoritative dropped item entity physics & lifecycle state.
#[derive(Component, Debug, Clone)]
pub struct DropState {
    pub position: [f64; 3],
    pub last_synced_pos: [f64; 3],
    pub velocity: [f64; 3],
    pub stack: ItemStack,
    pub pickup_delay: u16,
    pub age: u16,
    pub ticks_since_sync: u16,
}
