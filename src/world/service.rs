//! Unified world domain service: terrain template, sparse block edits,
//! cached per-chunk lighting, and 20 TPS clock/weather cycle.
use super::{
    BlockPos, BlockWorld, Snapshot, TickEvents, TimeSnapshot, WeatherEvent, WeatherSnapshot,
    WorldClock, affected_light_chunks, changed_blocks, chunk_payload, chunk_payload_with_light,
    has_any_edits,
};
use std::{
    collections::BTreeSet,
    io,
    sync::{Arc, Mutex},
};

/// Result of a world block mutation, computed under a single world lock and
/// returned without holding any lock so callers can encode and fan out safely.
#[derive(Debug, Clone)]
pub struct WorldMutation {
    pub block_changes: Vec<(BlockPos, i32)>,
    pub light_changes: Vec<([i32; 2], Arc<[u8]>)>,
}

/// Prepared chunk data plus snapshot metadata needed to reconcile concurrent
/// mutations that occur while the chunk is being written to the socket.
#[derive(Debug, Clone)]
pub(crate) struct PreparedChunk {
    pub snapshot: Snapshot,
    pub light: Arc<[u8]>,
    pub payload: Option<Arc<[u8]>>,
}

#[derive(Debug)]
pub struct World {
    terrain: Arc<[u8]>,
    blocks: Arc<BlockWorld>,
    clock: Mutex<WorldClock>,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    pub fn new() -> Self {
        Self {
            terrain: chunk_payload(),
            blocks: Arc::default(),
            clock: Mutex::new(WorldClock::new()),
        }
    }

    pub fn terrain(&self) -> &Arc<[u8]> {
        &self.terrain
    }

    pub fn block_at(&self, pos: BlockPos) -> i32 {
        self.blocks.lock().get(pos)
    }

    /// Prepare a chunk payload and light snapshot using the per-chunk light cache.
    pub(crate) fn prepare_chunk(&self, chunk: [i32; 2]) -> PreparedChunk {
        let mut blocks = self.blocks.lock();
        let neighborhood = blocks.neighborhood(chunk);
        let light = blocks.light_for(chunk, &neighborhood);
        let payload =
            has_any_edits(&neighborhood).then(|| chunk_payload_with_light(&neighborhood, &light));
        PreparedChunk {
            snapshot: neighborhood[1][1].clone(),
            light,
            payload,
        }
    }

    /// Reconcile block and light changes that occurred between `prepare_chunk`
    /// and finishing the socket write. Runs `on_reconciled` under the world lock
    /// so no concurrent write can slip between reconciliation and subscription.
    pub(crate) fn reconcile_and_subscribe<R>(
        &self,
        chunk: [i32; 2],
        old_snapshot: &Snapshot,
        old_light: &Arc<[u8]>,
        on_reconciled: impl FnOnce(Vec<(BlockPos, i32)>, Option<Arc<[u8]>>) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut blocks = self.blocks.lock();
        let neighborhood = blocks.neighborhood(chunk);
        let changes = changed_blocks(chunk, old_snapshot, &neighborhood[1][1]);
        let new_light = blocks.light_for(chunk, &neighborhood);
        let light_update = (new_light != *old_light).then_some(new_light);
        on_reconciled(changes, light_update)
    }

    /// Atomically set one block, refresh affected chunks' cached light once,
    /// and return all block + light diffs without holding the world lock.
    pub fn set_block(&self, pos: BlockPos, state: i32) -> Option<WorldMutation> {
        if !pos.valid() {
            return None;
        }
        let mut blocks = self.blocks.lock();
        let affected = affected_light_chunks(pos);
        let old_lights: Vec<_> = affected
            .iter()
            .map(|&nc| (nc, blocks.cached_or_current_light(nc)))
            .collect();
        if !blocks.set(pos, state) {
            return None;
        }
        let mut light_changes = Vec::new();
        for (nc, old_light) in old_lights {
            let new_light = blocks.recompute_light(nc);
            if new_light != old_light {
                light_changes.push((nc, new_light));
            }
        }
        Some(WorldMutation {
            block_changes: vec![(pos, state)],
            light_changes,
        })
    }

    /// Atomically fill a bounded axis-aligned box (up to 4,096 blocks) and
    /// refresh cached light for all affected chunks once.
    pub fn fill_blocks(
        &self,
        from: BlockPos,
        to: BlockPos,
        state: i32,
    ) -> io::Result<WorldMutation> {
        const MAX_FILL_BLOCKS: i64 = 4096;
        let min_x = from.0[0].min(to.0[0]);
        let max_x = from.0[0].max(to.0[0]);
        let min_y = from.0[1].min(to.0[1]);
        let max_y = from.0[1].max(to.0[1]);
        let min_z = from.0[2].min(to.0[2]);
        let max_z = from.0[2].max(to.0[2]);
        let start = BlockPos([min_x, min_y, min_z]);
        let end = BlockPos([max_x, max_y, max_z]);
        if !start.valid() || !end.valid() {
            return Err(crate::protocol::invalid(
                "fill coordinates are out of world bounds",
            ));
        }
        let volume = i64::from(max_x - min_x + 1)
            * i64::from(max_y - min_y + 1)
            * i64::from(max_z - min_z + 1);
        if volume > MAX_FILL_BLOCKS {
            return Err(crate::protocol::invalid("fill volume exceeds 4096 blocks"));
        }

        let mut blocks = self.blocks.lock();
        let mut light_chunks = BTreeSet::new();
        for x in min_x..=max_x {
            for z in min_z..=max_z {
                for nc in affected_light_chunks(BlockPos([x, min_y, z])) {
                    light_chunks.insert(nc);
                }
            }
        }
        let old_lights: Vec<_> = light_chunks
            .iter()
            .map(|&nc| (nc, blocks.cached_or_current_light(nc)))
            .collect();

        let mut block_changes = Vec::new();
        for y in min_y..=max_y {
            for z in min_z..=max_z {
                for x in min_x..=max_x {
                    let pos = BlockPos([x, y, z]);
                    if blocks.get(pos) == state {
                        continue;
                    }
                    if blocks.set(pos, state) {
                        block_changes.push((pos, state));
                    }
                }
            }
        }
        let mut light_changes = Vec::new();
        if !block_changes.is_empty() {
            for (nc, old_light) in old_lights {
                let new_light = blocks.recompute_light(nc);
                if new_light != old_light {
                    light_changes.push((nc, new_light));
                }
            }
        }
        Ok(WorldMutation {
            block_changes,
            light_changes,
        })
    }

    pub fn tick(&self) -> TickEvents {
        self.clock.lock().expect("world clock poisoned").tick()
    }

    pub fn time(&self) -> TimeSnapshot {
        self.clock.lock().expect("world clock poisoned").time()
    }

    pub fn weather(&self) -> WeatherSnapshot {
        self.clock.lock().expect("world clock poisoned").weather()
    }

    pub fn set_day_time(&self, day_time: i64) -> TimeSnapshot {
        self.clock
            .lock()
            .expect("world clock poisoned")
            .set_day_time(day_time)
    }

    pub fn set_weather(&self, raining: bool, thundering: bool, duration: u32) -> Vec<WeatherEvent> {
        self.clock
            .lock()
            .expect("world clock poisoned")
            .set_weather(raining, thundering, duration)
    }

    pub fn initial_environment(&self) -> (TimeSnapshot, Vec<WeatherEvent>) {
        self.clock
            .lock()
            .expect("world clock poisoned")
            .initial_state()
    }
}
