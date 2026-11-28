//! Unified world domain service: pluggable `ChunkGenerator`, lazy LRU `ChunkStore`,
//! cached per-chunk lighting, 20 TPS clock/weather cycle, and Anvil persistence.
use super::{
    AnvilStorage, BlockPos, Chunk, ChunkGenerator, ChunkStore, FlatGenerator, FlatGeneratorConfig,
    SavedPlayer, TickEvents, TimeSnapshot, WeatherEvent, WeatherSnapshot, WorldClock,
    affected_light_chunks, physics,
};
use crate::game::{Inventory, Player};
use std::{
    collections::{BTreeSet, HashMap},
    io,
    path::Path,
    sync::{Arc, Condvar, Mutex},
};
use uuid::Uuid;

#[derive(Debug, Default)]
struct InFlightSlot {
    /// `None` = still loading; `Some(Some(chunk))` = loaded; `Some(None)` = leader panicked.
    result: Mutex<Option<Option<Arc<Chunk>>>>,
    ready: Condvar,
}

struct InFlightGuard<'a> {
    map: &'a Mutex<HashMap<[i32; 2], Arc<InFlightSlot>>>,
    coords: [i32; 2],
    slot: Arc<InFlightSlot>,
    completed: bool,
}

impl InFlightGuard<'_> {
    fn complete(mut self, chunk: Arc<Chunk>) -> Arc<Chunk> {
        self.completed = true;
        {
            *self.slot.result.lock().expect("in-flight slot poisoned") = Some(Some(chunk.clone()));
        }
        self.map
            .lock()
            .expect("in-flight map poisoned")
            .remove(&self.coords);
        self.slot.ready.notify_all();
        chunk
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            if let Ok(mut guard) = self.slot.result.lock() {
                *guard = Some(None);
            }
            if let Ok(mut map) = self.map.lock() {
                map.remove(&self.coords);
            }
            self.slot.ready.notify_all();
        }
    }
}

/// Result of a world block mutation, computed under a single world lock and
/// returned without holding any lock so callers can encode and fan out safely.
#[derive(Debug, Clone)]
pub struct WorldMutation {
    pub block_changes: Vec<(BlockPos, i32)>,
    pub light_changes: Vec<([i32; 2], Arc<[u8]>)>,
}

pub const DIRTY_FLUSH_BATCH_SIZE: usize = 256;

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
    in_flight: Mutex<HashMap<[i32; 2], Arc<InFlightSlot>>>,
    clock: Mutex<WorldClock>,
    physics: Mutex<physics::PhysicsScheduler>,
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
            in_flight: Mutex::new(HashMap::new()),
            clock: Mutex::new(WorldClock::new()),
            physics: Mutex::new(physics::PhysicsScheduler::new()),
            save_lock: Mutex::new(()),
            spawn,
            placeholder_terrain,
        }
    }

    /// Open (or initialize) a persisted Anvil world directory (defaulting to Superflat).
    pub fn open(
        dir: impl AsRef<Path>,
        flat_config: Option<FlatGeneratorConfig>,
    ) -> io::Result<Self> {
        Self::open_with(dir, crate::config::WorldType::Flat, None, flat_config)
    }

    /// Open (or initialize) a persisted Anvil world directory with explicit
    /// generator type (`Flat` or `Overworld`) and optional seed.
    /// Existing `level.dat` generator settings take precedence on reload.
    pub fn open_with(
        dir: impl AsRef<Path>,
        world_type: crate::config::WorldType,
        seed: Option<i64>,
        flat_config: Option<FlatGeneratorConfig>,
    ) -> io::Result<Self> {
        let default_spec = match world_type {
            crate::config::WorldType::Overworld => {
                super::GeneratorSpec::overworld(seed.unwrap_or_else(rand::random))
            }
            crate::config::WorldType::Flat => {
                super::GeneratorSpec::flat(&flat_config.unwrap_or_default())
            }
        };
        Self::open_with_registry(dir, default_spec, &super::GeneratorRegistry::default())
    }

    /// Open (or initialize) a persisted Anvil world using a generic `GeneratorSpec`
    /// and `GeneratorRegistry`, allowing custom generators to persist and reload
    /// without modifying `World` or `AnvilStorage`.
    pub fn open_with_registry(
        dir: impl AsRef<Path>,
        default_spec: super::GeneratorSpec,
        registry: &super::GeneratorRegistry,
    ) -> io::Result<Self> {
        let world_dir = dir.as_ref();
        let storage = AnvilStorage::new(world_dir);
        let loaded = storage.load_level()?;

        let spec = loaded
            .as_ref()
            .and_then(|l| l.generator.clone())
            .unwrap_or(default_spec);
        let action = if loaded.is_some() {
            "Preparing level"
        } else {
            "Generating new level"
        };
        tracing::info!(
            world = %world_dir.display(),
            generator = %spec.id,
            seed = spec.seed,
            "{action}"
        );
        let generator = registry.build(&spec)?;
        let spawn = loaded
            .as_ref()
            .and_then(|l| l.spawn)
            .unwrap_or_else(|| generator.spawn_position());
        let clock = loaded
            .as_ref()
            .map_or_else(WorldClock::new, |l| WorldClock::from_persisted(l.clock));

        let placeholder_terrain = generator
            .default_terrain_payload()
            .cloned()
            .unwrap_or_else(|| Arc::from([]));
        let store = Mutex::new(ChunkStore::new(generator.clone(), Some(storage.clone())));

        let world = Self {
            generator,
            storage: Some(storage),
            store,
            in_flight: Mutex::new(HashMap::new()),
            clock: Mutex::new(clock),
            physics: Mutex::new(physics::PhysicsScheduler::new()),
            save_lock: Mutex::new(()),
            spawn,
            placeholder_terrain,
        };

        if loaded.is_none() {
            world.prepare_spawn_area()?;
        }

        Ok(world)
    }

    /// Pre-generate and persist the initial spawn chunk area (5x5 = 25 chunks)
    /// when creating a brand new world, matching vanilla 1.21.1 `spawnChunkRadius = 2`.
    fn prepare_spawn_area(&self) -> io::Result<()> {
        const SPAWN_CHUNK_RADIUS: i32 = 2;
        let [scx, scz] = super::chunk_at(self.spawn[0], self.spawn[2]);
        let side = (SPAWN_CHUNK_RADIUS * 2 + 1) as usize;
        let total = side * side;
        let started = std::time::Instant::now();
        let mut generated = 0usize;
        let mut last_bucket = 0usize;

        {
            let mut store = self.store.lock().expect("chunk store poisoned");
            for dz in -SPAWN_CHUNK_RADIUS..=SPAWN_CHUNK_RADIUS {
                for dx in -SPAWN_CHUNK_RADIUS..=SPAWN_CHUNK_RADIUS {
                    let _ = store.get_or_load([scx + dx, scz + dz]);
                    generated += 1;
                    let pct = (generated * 100) / total;
                    let bucket = pct / 25;
                    if bucket > last_bucket && bucket < 4 {
                        last_bucket = bucket;
                        tracing::info!("Preparing spawn area: {}%", bucket * 25);
                    }
                }
            }
        }

        let summary = self.save_all()?;
        tracing::info!(
            chunks = summary.chunks_saved,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Preparing spawn area: 100%"
        );
        Ok(())
    }

    pub fn generator(&self) -> &Arc<dyn ChunkGenerator> {
        &self.generator
    }

    pub fn generator_spec(&self) -> Option<super::GeneratorSpec> {
        self.generator.descriptor()
    }

    pub fn is_flat(&self) -> bool {
        self.generator.is_flat()
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

    /// True when dirty chunks have reached the incremental writeback threshold.
    pub fn needs_dirty_flush(&self) -> bool {
        self.store
            .lock()
            .expect("chunk store poisoned")
            .needs_dirty_flush()
    }

    #[cfg(test)]
    pub(crate) fn cached_chunk_len(&self) -> usize {
        self.store
            .lock()
            .expect("chunk store poisoned")
            .cached_len()
    }

    #[cfg(test)]
    pub(crate) fn dirty_chunk_len(&self) -> usize {
        self.store.lock().expect("chunk store poisoned").dirty_len()
    }

    /// Incrementally flush up to `max_chunks` oldest dirty chunks to Anvil `.mca`
    /// and immediately evict newly-clean entries if cache is above capacity.
    pub fn flush_dirty_batch(&self, max_chunks: usize) -> io::Result<usize> {
        let Some(storage) = &self.storage else {
            return Ok(0);
        };
        if max_chunks == 0 {
            return Ok(0);
        }
        let _save_guard = self.save_lock.lock().expect("world save lock poisoned");
        let batch = self
            .store
            .lock()
            .expect("chunk store poisoned")
            .take_oldest_dirty(max_chunks);
        if batch.is_empty() {
            return Ok(0);
        }
        let batch_coords: Vec<[i32; 2]> = batch.iter().map(|(c, _)| *c).collect();
        match storage.save_chunks(&batch) {
            Ok(saved) => {
                self.store
                    .lock()
                    .expect("chunk store poisoned")
                    .evict_if_needed();
                Ok(saved)
            }
            Err(error) => {
                self.store
                    .lock()
                    .expect("chunk store poisoned")
                    .mark_dirty(batch_coords);
                Err(error)
            }
        }
    }

    /// Apply synchronous writeback backpressure if dirty chunks have pushed the
    /// cache past `max_cached_chunks` despite clean LRU eviction.
    fn apply_cache_backpressure(&self) {
        let batch_size = {
            let store = self.store.lock().expect("chunk store poisoned");
            if !store.is_over_capacity() {
                return;
            }
            store.backpressure_batch_size()
        };
        if let Err(error) = self.flush_dirty_batch(batch_size) {
            tracing::error!(%error, "incremental dirty chunk backpressure flush failed");
        }
    }

    pub fn block_at(&self, pos: BlockPos) -> i32 {
        let chunk = self.ensure_chunk(pos.chunk());
        let (lx, y, lz) = pos.local();
        chunk.get_block(lx, y, lz)
    }

    /// Non-loading block lookup for game-loop hot paths (movement, item drops,
    /// block/fluid physics). Returns `None` if `pos` is out of bounds or its
    /// chunk is not currently cached in memory — never triggers disk I/O or
    /// terrain generation.
    pub fn block_at_if_loaded(&self, pos: BlockPos) -> Option<i32> {
        if !pos.valid() {
            return None;
        }
        let chunk = self
            .store
            .lock()
            .expect("chunk store poisoned")
            .peek_cached(pos.chunk())?;
        let (lx, y, lz) = pos.local();
        Some(chunk.get_block(lx, y, lz))
    }

    /// Preload a chunk into the in-memory LRU cache (intended to be called from
    /// `tokio::task::spawn_blocking` before dispatching cross-chunk game actions
    /// such as bed respawn verification).
    pub fn ensure_chunk_loaded(&self, coords: [i32; 2]) {
        let _ = self.ensure_chunk(coords);
    }

    /// Return `coords` from cache under a short lock, or load/generate it
    /// **outside** the store lock with per-chunk singleflight deduplication so
    /// concurrent callers approaching the same uncached chunk share one load/generation.
    fn ensure_chunk(&self, coords: [i32; 2]) -> Arc<Chunk> {
        enum Flight {
            Leader(Arc<InFlightSlot>),
            Follower(Arc<InFlightSlot>),
        }

        loop {
            {
                let mut store = self.store.lock().expect("chunk store poisoned");
                if let Some(chunk) = store.peek_cached(coords) {
                    return chunk;
                }
            }

            let flight = {
                let mut map = self.in_flight.lock().expect("in-flight map poisoned");
                if let Some(existing) = map.get(&coords) {
                    Flight::Follower(existing.clone())
                } else {
                    let slot = Arc::new(InFlightSlot::default());
                    map.insert(coords, slot.clone());
                    Flight::Leader(slot)
                }
            };

            let slot = match flight {
                Flight::Follower(slot) => {
                    let mut res = slot.result.lock().expect("in-flight slot poisoned");
                    while res.is_none() {
                        res = slot.ready.wait(res).expect("in-flight slot poisoned");
                    }
                    if let Some(Some(chunk)) = res.as_ref() {
                        return chunk.clone();
                    }
                    // Leader panicked before publishing a chunk; retry cleanly.
                    continue;
                }
                Flight::Leader(slot) => slot,
            };

            let guard = InFlightGuard {
                map: &self.in_flight,
                coords,
                slot,
                completed: false,
            };

            // Re-check cache once in case a prior leader committed right before we registered.
            {
                let mut store = self.store.lock().expect("chunk store poisoned");
                if let Some(chunk) = store.peek_cached(coords) {
                    return guard.complete(chunk);
                }
            }

            let loaded = super::store::load_or_generate_uncached(
                &*self.generator,
                self.storage.as_ref(),
                coords,
            );
            let chunk = self
                .store
                .lock()
                .expect("chunk store poisoned")
                .commit_loaded(coords, loaded);
            let chunk = guard.complete(chunk);
            self.apply_cache_backpressure();
            return chunk;
        }
    }

    /// Ensure all 9 chunks in the 3x3 neighborhood of `center` are loaded,
    /// performing all disk I/O and terrain generation outside the store lock
    /// with per-chunk singleflight deduplication.
    fn ensure_neighborhood(&self, [cx, cz]: [i32; 2]) -> (super::Neighborhood, Option<Arc<[u8]>>) {
        let mut slots: [[Option<Arc<Chunk>>; 3]; 3] = Default::default();
        let mut missing = Vec::new();
        let cached_light;
        {
            let mut store = self.store.lock().expect("chunk store poisoned");
            for (dz, row) in slots.iter_mut().enumerate() {
                for (dx, slot) in row.iter_mut().enumerate() {
                    let coords = [cx + dx as i32 - 1, cz + dz as i32 - 1];
                    if let Some(chunk) = store.peek_cached(coords) {
                        *slot = Some(chunk);
                    } else {
                        missing.push((dx, dz, coords));
                    }
                }
            }
            cached_light = store.peek_light([cx, cz]);
        }

        if missing.is_empty() {
            let neighborhood =
                std::array::from_fn(|dz| std::array::from_fn(|dx| slots[dz][dx].take().unwrap()));
            return (neighborhood, cached_light);
        }

        for (dx, dz, coords) in missing {
            slots[dz][dx] = Some(self.ensure_chunk(coords));
        }

        let cached_light = self
            .store
            .lock()
            .expect("chunk store poisoned")
            .peek_light([cx, cz]);

        let neighborhood =
            std::array::from_fn(|dz| std::array::from_fn(|dx| slots[dz][dx].take().unwrap()));
        (neighborhood, cached_light)
    }

    pub fn chest_items(
        &self,
        pos: BlockPos,
    ) -> Option<[Option<crate::game::ItemStack>; super::CHEST_SLOTS]> {
        if !pos.valid() {
            return None;
        }
        self.store
            .lock()
            .expect("chunk store poisoned")
            .chest_items(pos)
    }

    pub fn update_chest(
        &self,
        pos: BlockPos,
        mutate: impl FnOnce(&mut [Option<crate::game::ItemStack>; super::CHEST_SLOTS]) -> bool,
    ) -> Option<[Option<crate::game::ItemStack>; super::CHEST_SLOTS]> {
        if !pos.valid() {
            return None;
        }
        self.store
            .lock()
            .expect("chunk store poisoned")
            .update_chest(pos, mutate)
    }

    pub fn furnace_data(&self, pos: BlockPos) -> Option<super::FurnaceData> {
        if !pos.valid() {
            return None;
        }
        self.store
            .lock()
            .expect("chunk store poisoned")
            .furnace_data(pos)
    }

    pub fn update_furnace(
        &self,
        pos: BlockPos,
        mutate: impl FnOnce(&mut super::FurnaceData) -> bool,
    ) -> Option<super::FurnaceData> {
        if !pos.valid() {
            return None;
        }
        self.store
            .lock()
            .expect("chunk store poisoned")
            .update_furnace(pos, mutate)
    }

    /// Advance all loaded furnaces by 1 tick (20 TPS), applying any `lit` block
    /// state & lighting transitions. Returns `(updated_positions, mutations)`.
    pub fn tick_furnaces(&self) -> (Vec<BlockPos>, Vec<WorldMutation>) {
        let updates = self
            .store
            .lock()
            .expect("chunk store poisoned")
            .tick_furnaces();
        let mut changed_positions = Vec::with_capacity(updates.len());
        let mut mutations = Vec::new();
        for (pos, new_state) in updates {
            changed_positions.push(pos);
            if let Some(state) = new_state
                && let Some(mutation) = self.set_block(pos, state)
            {
                mutations.push(mutation);
            }
        }
        (changed_positions, mutations)
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

        let gen_spec = self.generator.descriptor();
        let write_result = (|| -> io::Result<usize> {
            storage.save_level(clock_state, gen_spec.as_ref(), self.spawn)?;
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

    /// Prepare a chunk payload and light snapshot using a short-lock query ->
    /// off-lock load/generate/light/encode -> short-lock commit pipeline so
    /// slow chunk I/O or lighting never serializes other players' world access.
    pub(crate) fn prepare_chunk(&self, chunk: [i32; 2]) -> PreparedChunk {
        let (neighborhood, cached_light) = self.ensure_neighborhood(chunk);
        let light = match cached_light {
            Some(light) => light,
            None => {
                let (computed, is_default) =
                    super::store::compute_light_uncached(&*self.generator, &neighborhood);
                self.store
                    .lock()
                    .expect("chunk store poisoned")
                    .commit_light_if_unchanged(chunk, &neighborhood, computed, is_default)
            }
        };
        let payload =
            super::store::encode_payload_uncached(&*self.generator, &neighborhood, &light);
        PreparedChunk {
            snapshot: neighborhood[1][1].clone(),
            light,
            payload,
        }
    }

    /// Reconcile block and light changes that occurred between `prepare_chunk`
    /// and finishing the socket write. Pre-warms the neighborhood and light
    /// outside the lock, then runs `on_reconciled` under a short world lock so
    /// no concurrent write can slip between reconciliation and subscription.
    pub(crate) fn reconcile_and_subscribe<R>(
        &self,
        chunk: [i32; 2],
        old_snapshot: &Arc<Chunk>,
        old_light: &Arc<[u8]>,
        on_reconciled: impl FnOnce(Vec<(BlockPos, i32)>, Option<Arc<[u8]>>) -> io::Result<R>,
    ) -> io::Result<R> {
        let (neighborhood, cached_light) = self.ensure_neighborhood(chunk);
        if cached_light.is_none() {
            let (computed, is_default) =
                super::store::compute_light_uncached(&*self.generator, &neighborhood);
            let _ = self
                .store
                .lock()
                .expect("chunk store poisoned")
                .commit_light_if_unchanged(chunk, &neighborhood, computed, is_default);
        }
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

    /// Advance scheduled block physics (gravity blocks and water/lava fluids) by 1 tick.
    pub fn tick_physics(&self) -> Vec<WorldMutation> {
        let due = self
            .physics
            .lock()
            .expect("physics scheduler poisoned")
            .pop_due();
        if due.is_empty() {
            return Vec::new();
        }
        let mut mutations = Vec::new();
        for (pos, kind) in due {
            if self.block_at_if_loaded(pos).is_none() {
                continue;
            }
            let edits = physics::evaluate_tick(pos, kind, |p| {
                self.block_at_if_loaded(p)
                    .unwrap_or(crate::game::BEDROCK_STATE)
            });
            for (epos, estate) in edits {
                if self.block_at_if_loaded(epos).is_none() {
                    continue;
                }
                if let Some(mutation) = self.set_block(epos, estate) {
                    mutations.push(mutation);
                }
            }
        }
        mutations
    }

    /// Atomically set one block, refresh affected chunks' cached light once,
    /// and return all block + light diffs without holding the world lock.
    pub fn set_block(&self, pos: BlockPos, state: i32) -> Option<WorldMutation> {
        if !pos.valid() {
            return None;
        }
        let (light_changes, samples) = {
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
            let mut samples = Vec::with_capacity(7);
            for npos in physics::neighborhood_positions(pos) {
                if npos.valid() {
                    samples.push((npos, store.get_block(npos)));
                }
            }
            (light_changes, samples)
        };
        self.physics
            .lock()
            .expect("physics scheduler poisoned")
            .schedule_samples(&samples);
        self.apply_cache_backpressure();
        Some(WorldMutation {
            block_changes: vec![(pos, state)],
            light_changes,
        })
    }

    /// Atomically fill a bounded axis-aligned box (up to 32,768 blocks, matching
    /// vanilla `commandModificationBlockLimit`) and refresh cached light once.
    pub fn fill_blocks(
        &self,
        from: BlockPos,
        to: BlockPos,
        state: i32,
    ) -> io::Result<WorldMutation> {
        const MAX_FILL_BLOCKS: i64 = 32_768;
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
            return Err(crate::protocol::invalid("fill volume exceeds 32768 blocks"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[derive(Debug)]
    struct BlockingGenerator {
        inner: FlatGenerator,
        slow_chunk: [i32; 2],
        entered: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl ChunkGenerator for BlockingGenerator {
        fn generate_chunk(&self, cx: i32, cz: i32) -> Arc<Chunk> {
            if [cx, cz] == self.slow_chunk {
                if let Some(tx) = self.entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = self.release.lock().unwrap().recv();
            }
            self.inner.generate_chunk(cx, cz)
        }

        fn spawn_position(&self) -> [f64; 3] {
            self.inner.spawn_position()
        }
    }

    #[test]
    fn prepare_chunk_does_not_hold_world_lock_during_chunk_generation() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let generator = Arc::new(BlockingGenerator {
            inner: FlatGenerator::default(),
            slow_chunk: [10, 10],
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(release_rx),
        });
        let world = Arc::new(World::with_generator(generator));

        // Pre-warm [0, 0] and its neighborhood before starting the slow request.
        let _ = world.prepare_chunk([0, 0]);

        let slow_world = world.clone();
        let slow_thread = std::thread::spawn(move || slow_world.prepare_chunk([10, 10]));

        // Wait until the slow thread is blocked inside `generate_chunk([10, 10])`.
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("slow generator should start");

        // While [10, 10] is generating off-lock, other world reads & writes on [0, 0]
        // must proceed immediately without deadlocking or blocking.
        assert!(
            world
                .set_block(BlockPos([1, 64, 1]), crate::game::GLASS_STATE)
                .is_some()
        );
        assert_eq!(
            world.block_at(BlockPos([1, 64, 1])),
            crate::game::GLASS_STATE
        );

        // Release the slow generator and confirm `prepare_chunk([10, 10])` finishes.
        release_tx.send(()).unwrap();
        let prepared = slow_thread.join().unwrap();
        assert!(!prepared.payload.is_empty());
    }

    #[test]
    fn dirty_chunks_exceeding_capacity_trigger_writeback_and_stay_bounded() {
        let dir = std::env::temp_dir().join(format!("zcraft-dirty-cap-{}", rand::random::<u64>()));
        let storage = AnvilStorage::new(&dir);
        let generator: Arc<dyn ChunkGenerator> = Arc::new(FlatGenerator::default());
        let capacity = 16usize;
        let world = World {
            generator: generator.clone(),
            storage: Some(storage.clone()),
            store: Mutex::new(ChunkStore::with_capacity(
                generator,
                Some(storage),
                capacity,
            )),
            in_flight: Mutex::new(HashMap::new()),
            clock: Mutex::new(WorldClock::new()),
            physics: Mutex::new(physics::PhysicsScheduler::new()),
            save_lock: Mutex::new(()),
            spawn: [8.5, 65.0, 8.5],
            placeholder_terrain: Arc::from([]),
        };

        // Explore and mutate 64 distinct chunks (4x cache capacity), so every chunk is dirty.
        for cx in 0..64 {
            let pos = BlockPos([cx * 16 + 2, 64, 2]);
            assert!(world.set_block(pos, crate::game::GLASS_STATE).is_some());
            assert!(
                world.cached_chunk_len() <= capacity,
                "cache grew to {} (capacity {capacity}) at chunk {cx}",
                world.cached_chunk_len()
            );
        }

        // Dirty count stayed bounded well below total explored chunks (64).
        assert!(world.dirty_chunk_len() <= capacity);

        // Reloading an early evicted chunk [0, 0] from disk must preserve its edited block.
        assert_eq!(
            world.block_at(BlockPos([2, 64, 2])),
            crate::game::GLASS_STATE
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_third_generator_persists_and_reloads_via_registry() {
        #[derive(Debug)]
        struct StonePlatformGenerator {
            seed: i64,
            y: i32,
        }
        impl ChunkGenerator for StonePlatformGenerator {
            fn generate_chunk(&self, _cx: i32, _cz: i32) -> Arc<Chunk> {
                let mut chunk = Chunk::empty(0);
                for z in 0..16 {
                    for x in 0..16 {
                        chunk.set_block(x, self.y, z, crate::game::STONE_STATE);
                    }
                }
                Arc::new(chunk)
            }
            fn spawn_position(&self) -> [f64; 3] {
                [0.5, f64::from(self.y + 1), 0.5]
            }
            fn descriptor(&self) -> Option<crate::world::GeneratorSpec> {
                Some(crate::world::GeneratorSpec {
                    id: "zcraft:stone_platform".into(),
                    seed: self.seed,
                    settings: serde_json::json!({ "y": self.y }),
                })
            }
        }

        let mut reg = crate::world::GeneratorRegistry::with_builtins();
        reg.register("zcraft:stone_platform", |spec| {
            let y = spec
                .settings
                .get("y")
                .and_then(|v| v.as_i64())
                .unwrap_or(64) as i32;
            Ok(Arc::new(StonePlatformGenerator { seed: spec.seed, y }))
        });

        let dir = std::env::temp_dir().join(format!("zcraft-custom-gen-{}", rand::random::<u64>()));
        let spec = crate::world::GeneratorSpec {
            id: "zcraft:stone_platform".into(),
            seed: 42,
            settings: serde_json::json!({ "y": 72 }),
        };
        {
            let world = World::open_with_registry(&dir, spec.clone(), &reg).unwrap();
            assert_eq!(
                world.block_at(BlockPos([3, 72, 3])),
                crate::game::STONE_STATE
            );
            assert!(!world.is_flat());
            world.save_all().unwrap();
        }
        // Reopen with a different fallback spec; level.dat must restore the custom generator.
        let fallback = crate::world::GeneratorSpec::flat(&FlatGeneratorConfig::default());
        let reopened = World::open_with_registry(&dir, fallback, &reg).unwrap();
        assert_eq!(reopened.generator_spec(), Some(spec));
        assert_eq!(
            reopened.block_at(BlockPos([50, 72, 50])),
            crate::game::STONE_STATE
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_chunk_requests_deduplicate_generation_via_singleflight() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };

        #[derive(Debug)]
        struct CountingGenerator {
            calls: Arc<AtomicUsize>,
        }
        impl ChunkGenerator for CountingGenerator {
            fn generate_chunk(&self, _cx: i32, _cz: i32) -> Arc<Chunk> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(25));
                Arc::new(Chunk::empty(0))
            }
            fn spawn_position(&self) -> [f64; 3] {
                [8.5, 65.0, 8.5]
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let world = Arc::new(World::with_generator(Arc::new(CountingGenerator {
            calls: calls.clone(),
        })));
        let threads = 6usize;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let world = world.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                world.block_at(BlockPos([128, 64, 128]))
            }));
        }
        for h in handles {
            assert_eq!(h.join().unwrap(), crate::game::AIR_STATE);
        }
        // All 6 concurrent threads targeted chunk [8, 8]; singleflight must
        // execute `generate_chunk` exactly once.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn block_at_if_loaded_never_triggers_generation_for_uncached_chunks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct CountingGenerator {
            calls: Arc<AtomicUsize>,
        }
        impl ChunkGenerator for CountingGenerator {
            fn generate_chunk(&self, _cx: i32, _cz: i32) -> Arc<Chunk> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Arc::new(Chunk::empty(0))
            }
            fn spawn_position(&self) -> [f64; 3] {
                [8.5, 65.0, 8.5]
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let world = World::with_generator(Arc::new(CountingGenerator {
            calls: calls.clone(),
        }));
        assert_eq!(world.block_at_if_loaded(BlockPos([500, 64, 500])), None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        world.ensure_chunk_loaded(BlockPos([500, 64, 500]).chunk());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            world.block_at_if_loaded(BlockPos([500, 64, 500])),
            Some(crate::game::AIR_STATE)
        );
    }
}
