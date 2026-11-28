//! ECS Resources for the voxel world, entity indexes, container viewers, and IO queues.
use super::io::{GameInput, GameOutput};
use crate::world::{BlockPos, World as VoxelTerrain};
use bevy_ecs::prelude::{Entity, Resource};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use uuid::Uuid;

pub const MAX_ITEM_DROPS: usize = 512;

/// Shared voxel terrain, chunk store, lighting, clock/weather, and persistence service.
#[derive(Resource, Clone)]
pub struct VoxelWorld(pub Arc<VoxelTerrain>);

/// Fast entity lookup index + entity ID allocator.
#[derive(Resource)]
pub struct EntityRegistry {
    pub players: HashMap<Uuid, Entity>,
    /// Ordered FIFO list of active item drop entities so oldest can be evicted at `MAX_ITEM_DROPS`.
    pub drops: VecDeque<Entity>,
    pub next_entity_id: Arc<AtomicI32>,
}

impl EntityRegistry {
    pub fn new(next_entity_id: Arc<AtomicI32>) -> Self {
        Self {
            players: HashMap::new(),
            drops: VecDeque::new(),
            next_entity_id,
        }
    }

    pub fn allocate_entity_id(&self) -> Option<i32> {
        self.next_entity_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()
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
