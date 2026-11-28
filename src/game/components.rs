//! Granular, pure-data ECS Components for authoritative gameplay entities.
//!
//! Entities are composed from fine-grained shared components (`Position`,
//! `Rotation`, `Velocity`, `NetworkEntityId`, `EntityUuid`) and dynamically
//! attached state components (`Dead`, `EatingState`, `MiningState`) rather than
//! monolithic entity structs.
use super::io::OpenContainer;
use crate::{
    config::GameMode,
    game::{
        CREATIVE_BLOCK_REACH, HURT_INVULNERABILITY_TICKS, Hand, Inventory, Item, ItemStack,
        PLAYER_EYE_HEIGHT, PLAYER_HALF_WIDTH, PLAYER_HEIGHT, SURVIVAL_BLOCK_REACH,
    },
    protocol::packets::play_entities::EntityPose,
    world::BlockPos,
};
use bevy_ecs::prelude::Component;
use std::collections::HashSet;
use uuid::Uuid;

// ============================================================================
// Entity Markers
// ============================================================================

/// Marker component for online player entities.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct PlayerMarker;

/// Marker component for dropped item entities in the world.
#[derive(Component, Debug, Default, Clone, Copy)]
pub struct ItemDropMarker;

// ============================================================================
// Shared Cross-Entity Identity & Spatial Components
// ============================================================================

/// Protocol-visible entity ID shared by players and world entities.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkEntityId(pub i32);

/// Stable UUID for entities.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityUuid(pub Uuid);

/// World-space 3D position shared by players, item drops, and future entities.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq)]
pub struct Position(pub [f64; 3]);

/// Entity orientation in degrees (yaw, pitch).
#[derive(Component, Debug, Default, Clone, Copy, PartialEq)]
pub struct Rotation {
    pub yaw: f32,
    pub pitch: f32,
}

/// World-space 3D velocity in blocks/tick.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq)]
pub struct Velocity(pub [f64; 3]);

// ============================================================================
// Granular Player Components
// ============================================================================

/// Low-frequency player identity, game mode, and respawn metadata.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PlayerProfile {
    pub name: String,
    pub game_mode: GameMode,
    pub respawn_point: Option<BlockPos>,
}

impl PlayerProfile {
    pub fn is_creative(&self) -> bool {
        self.game_mode == GameMode::Creative
    }

    pub fn is_survival(&self) -> bool {
        self.game_mode == GameMode::Survival
    }

    pub fn can_interact(&self) -> bool {
        matches!(self.game_mode, GameMode::Creative | GameMode::Survival)
    }

    pub fn block_reach(&self) -> f64 {
        if self.is_creative() {
            CREATIVE_BLOCK_REACH
        } else {
            SURVIVAL_BLOCK_REACH
        }
    }
}

/// Movement posture flags and accumulated fall distance.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq)]
pub struct MovementState {
    pub on_ground: bool,
    pub sneaking: bool,
    pub sprinting: bool,
    pub fall_distance: f64,
}

/// Living entity health and hurt invulnerability timestamp (in ticks).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Health {
    pub current: f32,
    pub last_hurt_tick: Option<u64>,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            current: 20.0,
            last_hurt_tick: None,
        }
    }
}

/// Vanilla `FoodData` state (hunger, saturation, exhaustion, regen/starvation timer).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Hunger {
    pub food: i32,
    pub saturation: f32,
    pub exhaustion: f32,
    pub tick_timer: u16,
}

impl Default for Hunger {
    fn default() -> Self {
        Self {
            food: 20,
            saturation: 5.0,
            exhaustion: 0.0,
            tick_timer: 0,
        }
    }
}

/// Authoritative 46-slot player inventory.
#[derive(Component, Debug, Clone)]
pub struct PlayerInventory(pub Inventory);

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

// ============================================================================
// Dynamic / Pluggable State Components (inserted only while active)
// ============================================================================

/// Zero-sized marker component attached while an entity is dead.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dead;

/// Attached while a player is actively eating a food item.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EatingState {
    pub hand: Hand,
    pub item: Item,
    pub start_tick: u64,
    pub required_ticks: u16,
}

/// Attached while a Survival-mode player is actively mining a block.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MiningState {
    pub target: BlockPos,
    pub start_tick: u64,
}

// ============================================================================
// Granular Item Drop Components
// ============================================================================

/// Item stack carried by a dropped item entity.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStackComp(pub ItemStack);

/// Age and pickup cooldown (in ticks) of a dropped item entity.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropLifecycle {
    pub age: u16,
    pub pickup_delay: u16,
}

/// Throttled network position sync state for a dropped item entity.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct DropSyncState {
    pub last_synced_pos: [f64; 3],
    pub ticks_since_sync: u16,
}

// ============================================================================
// Pure Stateless Gameplay Helpers over Granular Components
// ============================================================================

pub fn entity_pose(pos: Position, rot: Rotation, movement: MovementState) -> EntityPose {
    EntityPose::new(pos.0, rot.yaw, rot.pitch, movement.on_ground)
}

pub fn player_eye_position(pos: Position) -> [f64; 3] {
    [pos.0[0], pos.0[1] + PLAYER_EYE_HEIGHT, pos.0[2]]
}

pub fn player_in_reach(pos: Position, profile: &PlayerProfile, target: BlockPos) -> bool {
    let reach = profile.block_reach();
    player_eye_position(pos)
        .into_iter()
        .zip(target.0)
        .map(|(p, b)| {
            let b = f64::from(b);
            (p - p.clamp(b, b + 1.0)).powi(2)
        })
        .sum::<f64>()
        <= reach * reach
}

pub fn player_intersects_block(pos: Position, target: BlockPos) -> bool {
    let [x, y, z] = pos.0;
    let [bx, by, bz] = target.0.map(f64::from);
    x + PLAYER_HALF_WIDTH > bx
        && x - PLAYER_HALF_WIDTH < bx + 1.0
        && y + PLAYER_HEIGHT > by
        && y < by + 1.0
        && z + PLAYER_HALF_WIDTH > bz
        && z - PLAYER_HALF_WIDTH < bz + 1.0
}

/// Accumulate vanilla `FoodData` exhaustion (clamped at 40.0).
pub fn add_exhaustion(hunger: &mut Hunger, is_survival: bool, is_dead: bool, amount: f32) {
    if is_survival && !is_dead && amount > 0.0 && amount.is_finite() {
        hunger.exhaustion = (hunger.exhaustion + amount).min(40.0);
    }
}

/// Apply Survival damage respecting the 10-tick hurt invulnerability window.
/// Returns `(damaged, died)`.
pub fn apply_damage_at_tick(
    health: &mut Health,
    movement: &mut MovementState,
    is_survival: bool,
    is_dead: bool,
    amount: f32,
    tick: u64,
) -> (bool, bool) {
    if !is_survival || is_dead || amount <= 0.0 || !amount.is_finite() {
        return (false, false);
    }
    if let Some(last) = health.last_hurt_tick
        && tick.saturating_sub(last) < HURT_INVULNERABILITY_TICKS
    {
        return (false, false);
    }
    health.current = (health.current - amount).max(0.0);
    health.last_hurt_tick = Some(tick);
    let died = health.current <= 0.0;
    if died {
        movement.fall_distance = 0.0;
    }
    (true, died)
}

/// Advance vanilla `FoodData.tick()` by 1 tick for a living Survival player.
/// Returns `(vitals_changed, starved_hurt)`.
pub fn tick_hunger_and_regen(health: &mut Health, hunger: &mut Hunger) -> (bool, bool) {
    let prev = (health.current, hunger.food, hunger.saturation);
    let mut starved_hurt = false;

    if hunger.exhaustion > 4.0 {
        hunger.exhaustion -= 4.0;
        if hunger.saturation > 0.0 {
            hunger.saturation = (hunger.saturation - 1.0).max(0.0);
        } else {
            hunger.food = (hunger.food - 1).max(0);
        }
    }

    if hunger.saturation > 0.0 && health.current < 20.0 && hunger.food >= 20 {
        hunger.tick_timer += 1;
        if hunger.tick_timer >= 10 {
            let boost = hunger.saturation.min(6.0);
            health.current = (health.current + boost / 6.0).min(20.0);
            hunger.exhaustion = (hunger.exhaustion + boost).min(40.0);
            hunger.tick_timer = 0;
        }
    } else if hunger.food >= 18 && health.current < 20.0 {
        hunger.tick_timer += 1;
        if hunger.tick_timer >= 80 {
            health.current = (health.current + 1.0).min(20.0);
            hunger.exhaustion = (hunger.exhaustion + 6.0).min(40.0);
            hunger.tick_timer = 0;
        }
    } else if hunger.food <= 0 {
        hunger.tick_timer += 1;
        if hunger.tick_timer >= 80 {
            if health.current > 1.0 {
                health.current = (health.current - 1.0).max(1.0);
                starved_hurt = true;
            }
            hunger.tick_timer = 0;
        }
    } else {
        hunger.tick_timer = 0;
    }

    let changed = (health.current, hunger.food, hunger.saturation) != prev;
    (changed, starved_hurt)
}
