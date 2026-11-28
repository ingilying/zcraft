//! ECS Resources for the voxel world, entity indexes, container viewers, and IO queues.
use super::io::{GameInput, GameOutput};
use crate::world::{BlockPos, World as VoxelTerrain};
use bevy_ecs::prelude::{Entity, Resource};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

pub const MAX_ITEM_DROPS: usize = 512;

/// Shared voxel terrain, chunk store, lighting, clock/weather, and persistence service.
#[derive(Resource, Clone)]
pub struct VoxelWorld(pub Arc<VoxelTerrain>);

#[derive(Debug)]
struct EntityIdState {
    next: i32,
    active: HashSet<i32>,
}

/// Thread-safe protocol entity ID allocator (`1..=i32::MAX`) that wraps safely
/// around `i32::MAX` while skipping any currently active entity IDs.
#[derive(Debug)]
pub struct EntityIdAllocator {
    state: Mutex<EntityIdState>,
}

impl Default for EntityIdAllocator {
    fn default() -> Self {
        Self::new(1)
    }
}

impl EntityIdAllocator {
    pub fn new(start: i32) -> Self {
        Self {
            state: Mutex::new(EntityIdState {
                next: start.max(1),
                active: HashSet::new(),
            }),
        }
    }

    pub fn allocate(&self) -> Option<i32> {
        let mut state = self.state.lock().expect("entity id allocator poisoned");
        if state.active.len() >= i32::MAX as usize {
            return None;
        }
        loop {
            let id = state.next;
            state.next = if id == i32::MAX { 1 } else { id + 1 };
            if state.active.insert(id) {
                return Some(id);
            }
        }
    }

    pub fn release(&self, id: i32) {
        if let Ok(mut state) = self.state.lock() {
            state.active.remove(&id);
        }
    }
}

/// Fast player entity lookup index + wrapping protocol entity ID allocator.
#[derive(Resource)]
pub struct EntityRegistry {
    pub players: HashMap<Uuid, Entity>,
    pub entity_ids: Arc<EntityIdAllocator>,
}

impl EntityRegistry {
    pub fn new(entity_ids: Arc<EntityIdAllocator>) -> Self {
        Self {
            players: HashMap::new(),
            entity_ids,
        }
    }

    pub fn allocate_entity_id(&self) -> Option<i32> {
        self.entity_ids.allocate()
    }

    pub fn release_entity_id(&self, id: i32) {
        self.entity_ids.release(id);
    }
}

/// Tracks which players currently have each block-entity container (chest / furnace) open.
#[derive(Resource, Default)]
pub struct ChestViewers(pub HashMap<BlockPos, HashSet<Uuid>>);

impl ChestViewers {
    pub fn open(&mut self, pos: BlockPos, uuid: Uuid) -> usize {
        let entry = self.0.entry(pos).or_default();
        entry.insert(uuid);
        entry.len()
    }

    pub fn close(&mut self, pos: BlockPos, uuid: Uuid) -> usize {
        if let Some(entry) = self.0.get_mut(&pos) {
            entry.remove(&uuid);
            let count = entry.len();
            if count == 0 {
                self.0.remove(&pos);
            }
            count
        } else {
            0
        }
    }

    pub fn remove_player(&mut self, uuid: Uuid) -> Vec<(BlockPos, usize)> {
        let mut changed = Vec::new();
        self.0.retain(|&pos, set| {
            if set.remove(&uuid) {
                changed.push((pos, set.len()));
            }
            !set.is_empty()
        });
        changed
    }

    pub fn others(&self, pos: BlockPos, exclude: Uuid) -> Vec<Uuid> {
        self.0
            .get(&pos)
            .map(|set| set.iter().copied().filter(|&id| id != exclude).collect())
            .unwrap_or_default()
    }
}

/// Authoritative discrete 20 TPS game-loop tick counter.
/// All gameplay durations (mining, eating, hurt invulnerability) derive from
/// this monotonic counter rather than wall-clock instants.
#[derive(Resource, Debug, Default, Clone, Copy)]
pub struct TickClock {
    pub tick: u64,
}

/// Inbound actions queued for the current ECS step.
#[derive(Resource, Default)]
pub struct InputQueue(pub Vec<GameInput>);

/// Outbound domain sync events emitted during the current ECS step.
#[derive(Resource, Default)]
pub struct OutputQueue(pub Vec<GameOutput>);

impl OutputQueue {
    pub fn push(&mut self, output: GameOutput) {
        self.0.push(output);
    }

    pub fn drain(&mut self) -> Vec<GameOutput> {
        std::mem::take(&mut self.0)
    }
}
