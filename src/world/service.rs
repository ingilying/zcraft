//! Unified world domain service: pluggable `ChunkGenerator`, lazy LRU `ChunkStore`,
//! cached per-chunk lighting, 20 TPS clock/weather cycle, and Anvil persistence.
use super::{
    AnvilStorage, BlockPos, Chunk, ChunkGenerator, ChunkStore, FlatGenerator, FlatGeneratorConfig,
    SavedPlayer, TickEvents, TimeSnapshot, WeatherEvent, WeatherSnapshot, WorldClock,
    affected_light_chunks,
};
use crate::game::{Inventory, Player};
use std::{
    collections::BTreeSet,
    io,
    path::Path,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// Result of a world block mutation, computed under a single world lock and
/// returned without holding any lock so callers can encode and fan out safely.
#[derive(Debug, Clone)]
pub struct WorldMutation {
    pub block_changes: Vec<(BlockPos, i32)>,
    pub light_changes: Vec<([i32; 2], Arc<[u8]>)>,
}

/// Summary of a world persistence flush (`save_all`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveSummary {
    pub persisted: bool,
    pub chunks_saved: usize,
}

/// Prepared chunk data plus snapshot metadata needed to reconcile concurrent
/// mutations that occur while the chunk is being written to the socket.
#[derive(Debug, Clone)]
pub(crate) struct PreparedChunk {
    pub snapshot: Arc<Chunk>,
    pub light: Arc<[u8]>,
    pub payload: Arc<[u8]>,
}

#[derive(Debug)]
pub struct World {
    generator: Arc<dyn ChunkGenerator>,
    storage: Option<AnvilStorage>,
    store: Mutex<ChunkStore>,
    clock: Mutex<WorldClock>,
    save_lock: Mutex<()>,
    spawn: [f64; 3],
    placeholder_terrain: Arc<[u8]>,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    pub fn new() -> Self {
        Self::with_generator(Arc::new(FlatGenerator::default()))
    }

    pub fn with_generator(generator: Arc<dyn ChunkGenerator>) -> Self {
        let spawn = generator.spawn_position();
        let placeholder_terrain = generator
            .default_terrain_payload()
            .cloned()
            .unwrap_or_else(|| Arc::from([]));
        let store = Mutex::new(ChunkStore::new(generator.clone(), None));
        Self {
            generator,
            storage: None,
            store,
            clock: Mutex::new(WorldClock::new()),
            save_lock: Mutex::new(()),
            spawn,
            placeholder_terrain,
        }
    }

    /// Open (or initialize) a persisted Anvil world directory.
    /// Existing `level.dat` generator settings take precedence over `flat_config`.
    /// Chunks are loaded lazily from `region/*.mca` on demand (O(1) startup).
    pub fn open(
        dir: impl AsRef<Path>,
        flat_config: Option<FlatGeneratorConfig>,
    ) -> io::Result<Self> {
        let storage = AnvilStorage::new(dir.as_ref());
        let loaded = storage.load_level()?;

        let effective_flat = loaded
            .as_ref()
            .and_then(|l| l.flat.clone())
            .or(flat_config)
            .unwrap_or_default();
        let flat_gen = FlatGenerator::from_config(effective_flat)?;
        let spawn = loaded
            .as_ref()
            .and_then(|l| l.spawn)
            .unwrap_or_else(|| flat_gen.spawn_position());
        let clock = loaded
            .as_ref()
            .map_or_else(WorldClock::new, |l| WorldClock::from_persisted(l.clock));

        if loaded.is_none() {
            storage.save_level(clock.persisted_state(), Some(flat_gen.config()), spawn)?;
        }

        let generator: Arc<dyn ChunkGenerator> = Arc::new(flat_gen);
        let placeholder_terrain = generator
            .default_terrain_payload()
            .cloned()
            .unwrap_or_else(|| Arc::from([]));
        let store = Mutex::new(ChunkStore::new(generator.clone(), Some(storage.clone())));

        Ok(Self {
            generator,
            storage: Some(storage),
            store,
            clock: Mutex::new(clock),
            save_lock: Mutex::new(()),
            spawn,
            placeholder_terrain,
        })
    }

    pub fn generator(&self) -> &Arc<dyn ChunkGenerator> {
        &self.generator
    }

    pub fn flat_config(&self) -> Option<&FlatGeneratorConfig> {
        self.generator.flat_config()
    }

    /// Placeholder terrain buffer used by `ChunkStream::pump` before
    /// `prepare_chunk` fills in the authoritative per-chunk payload.
    pub fn terrain(&self) -> &Arc<[u8]> {
        &self.placeholder_terrain
    }

    pub fn spawn_position(&self) -> [f64; 3] {
        self.spawn
    }

    pub fn has_storage(&self) -> bool {
        self.storage.is_some()
    }

    pub fn block_at(&self, pos: BlockPos) -> i32 {
        self.store
            .lock()
            .expect("chunk store poisoned")
            .get_block(pos)
    }

    /// Persist `level.dat` and all dirty chunks to Anvil `region/*.mca`.
    /// Serialized across concurrent callers so an older snapshot can never
    /// overwrite a newer one, and restores dirty markers on any I/O failure.
    pub fn save_all(&self) -> io::Result<SaveSummary> {
        let Some(storage) = &self.storage else {
            return Ok(SaveSummary {
                persisted: false,
                chunks_saved: 0,
            });
        };
        let _save_guard = self.save_lock.lock().expect("world save lock poisoned");
        let clock_state = self
            .clock
            .lock()
            .expect("world clock poisoned")
            .persisted_state();
        let dirty = self
            .store
            .lock()
            .expect("chunk store poisoned")
            .drain_dirty();
        let dirty_coords: Vec<[i32; 2]> = dirty.iter().map(|(c, _)| *c).collect();

        let write_result = (|| -> io::Result<usize> {
            storage.save_level(clock_state, self.generator.flat_config(), self.spawn)?;
            storage.save_chunks(&dirty)
        })();

        match write_result {
            Ok(chunks_saved) => Ok(SaveSummary {
                persisted: true,
                chunks_saved,
            }),
            Err(error) => {
                self.store
                    .lock()
                    .expect("chunk store poisoned")
                    .mark_dirty(dirty_coords);
                Err(error)
            }
        }
    }

    pub fn load_player(&self, uuid: Uuid) -> io::Result<Option<SavedPlayer>> {
        match &self.storage {
            Some(storage) => storage.load_player(uuid),
            None => Ok(None),
        }
    }

    pub fn save_player(&self, player: &Player, inventory: &Inventory) -> io::Result<()> {
        match &self.storage {
            Some(storage) => storage.save_player(player, inventory),
            None => Ok(()),
        }
    }

    /// Prepare a chunk payload and light snapshot using the lazy chunk store
    /// and per-chunk light cache.
    pub(crate) fn prepare_chunk(&self, chunk: [i32; 2]) -> PreparedChunk {
        let mut store = self.store.lock().expect("chunk store poisoned");
        let neighborhood = store.neighborhood(chunk);
        let light = store.light_for(chunk, &neighborhood);
        let payload = store.payload_for(&neighborhood, &light);
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
        old_snapshot: &Arc<Chunk>,
        old_light: &Arc<[u8]>,
        on_reconciled: impl FnOnce(Vec<(BlockPos, i32)>, Option<Arc<[u8]>>) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut store = self.store.lock().expect("chunk store poisoned");
        let neighborhood = store.neighborhood(chunk);
        let current = &neighborhood[1][1];
        let changes = if Arc::ptr_eq(old_snapshot, current) {
            Vec::new()
        } else {
            current.diff_blocks(chunk, old_snapshot)
        };
        let new_light = store.light_for(chunk, &neighborhood);
        let light_update = (new_light != *old_light).then_some(new_light);
        on_reconciled(changes, light_update)
    }

    /// Atomically set one block, refresh affected chunks' cached light once,
    /// and return all block + light diffs without holding the world lock.
    pub fn set_block(&self, pos: BlockPos, state: i32) -> Option<WorldMutation> {
        if !pos.valid() {
            return None;
        }
        let mut store = self.store.lock().expect("chunk store poisoned");
        let affected = affected_light_chunks(pos);
        let old_lights: Vec<_> = affected
            .iter()
            .map(|&nc| (nc, store.cached_or_current_light(nc)))
            .collect();
        if !store.set_block(pos, state) {
            return None;
        }
        let mut light_changes = Vec::new();
        for (nc, old_light) in old_lights {
            let new_light = store.recompute_light(nc);
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

        let mut store = self.store.lock().expect("chunk store poisoned");
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
            .map(|&nc| (nc, store.cached_or_current_light(nc)))
            .collect();

        let mut block_changes = Vec::new();
        for y in min_y..=max_y {
            for z in min_z..=max_z {
                for x in min_x..=max_x {
                    let pos = BlockPos([x, y, z]);
                    if store.set_block(pos, state) {
                        block_changes.push((pos, state));
                    }
                }
            }
        }
        let mut light_changes = Vec::new();
        if !block_changes.is_empty() {
            for (nc, old_light) in old_lights {
                let new_light = store.recompute_light(nc);
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
