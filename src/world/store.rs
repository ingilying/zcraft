//! Lazy-loading, copy-on-write, LRU-bounded chunk cache with per-chunk lighting
//! and dirty tracking for Anvil persistence.
use super::{
    AnvilStorage, BlockPos, Chunk, ChunkGenerator, Neighborhood, chunk_light, is_uniform_template,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub const DEFAULT_MAX_CACHED_CHUNKS: usize = 4096;

#[derive(Debug)]
pub(crate) struct ChunkStore {
    generator: Arc<dyn ChunkGenerator>,
    storage: Option<AnvilStorage>,
    chunks: HashMap<[i32; 2], Arc<Chunk>>,
    light_cache: HashMap<[i32; 2], Arc<[u8]>>,
    dirty: HashSet<[i32; 2]>,
    access_order: HashMap<[i32; 2], u64>,
    access_clock: u64,
    max_cached_chunks: usize,
}

impl ChunkStore {
    pub(crate) fn new(generator: Arc<dyn ChunkGenerator>, storage: Option<AnvilStorage>) -> Self {
        Self::with_capacity(generator, storage, DEFAULT_MAX_CACHED_CHUNKS)
    }

    pub(crate) fn with_capacity(
        generator: Arc<dyn ChunkGenerator>,
        storage: Option<AnvilStorage>,
        max_cached_chunks: usize,
    ) -> Self {
        Self {
            generator,
            storage,
            chunks: HashMap::new(),
            light_cache: HashMap::new(),
            dirty: HashSet::new(),
            access_order: HashMap::new(),
            access_clock: 0,
            max_cached_chunks: max_cached_chunks.max(16),
        }
    }

    #[cfg(test)]
    pub(crate) fn cached_len(&self) -> usize {
        self.chunks.len()
    }

    fn touch(&mut self, coords: [i32; 2]) {
        self.access_clock = self.access_clock.wrapping_add(1);
        self.access_order.insert(coords, self.access_clock);
    }

    fn evict_if_needed(&mut self) {
        if self.chunks.len() <= self.max_cached_chunks {
            return;
        }
        let target = self.max_cached_chunks - self.max_cached_chunks / 8;
        let mut candidates: Vec<([i32; 2], u64)> = self
            .chunks
            .keys()
            .filter(|coords| !self.dirty.contains(*coords))
            .map(|&coords| (coords, self.access_order.get(&coords).copied().unwrap_or(0)))
            .collect();
        if candidates.is_empty() {
            return;
        }
        candidates.sort_unstable_by_key(|&(_, tick)| tick);
        let to_remove = self.chunks.len().saturating_sub(target);
        for (coords, _) in candidates.into_iter().take(to_remove) {
            self.chunks.remove(&coords);
            self.access_order.remove(&coords);
            self.light_cache.remove(&coords);
        }
    }

    /// Return the chunk at `coords`, lazily loading from Anvil `.mca` on first
    /// access or generating via `ChunkGenerator` if absent on disk.
    pub(crate) fn get_or_load(&mut self, coords: [i32; 2]) -> Arc<Chunk> {
        if let Some(chunk) = self.chunks.get(&coords).cloned() {
            self.touch(coords);
            return chunk;
        }
        let loaded = self.storage.as_ref().and_then(|storage| {
            match storage.load_chunk(coords[0], coords[1]) {
                Ok(Some(chunk)) => Some(Arc::new(chunk)),
                Ok(None) => None,
                Err(error) => {
                    tracing::error!(
                        cx = coords[0],
                        cz = coords[1],
                        %error,
                        "failed to load Anvil chunk; falling back to generator"
                    );
                    None
                }
            }
        });
        let (chunk, newly_generated) = match loaded {
            Some(chunk) => (chunk, false),
            None => (self.generator.generate_chunk(coords[0], coords[1]), true),
        };
        self.chunks.insert(coords, chunk.clone());
        if newly_generated && self.storage.is_some() {
            self.dirty.insert(coords);
        }
        self.touch(coords);
        self.evict_if_needed();
        chunk
    }

    pub(crate) fn neighborhood(&mut self, [cx, cz]: [i32; 2]) -> Neighborhood {
        std::array::from_fn(|dz| {
            std::array::from_fn(|dx| self.get_or_load([cx + dx as i32 - 1, cz + dz as i32 - 1]))
        })
    }

    fn fast_path(&self) -> Option<(&Arc<Chunk>, &Arc<[u8]>)> {
        Some((
            self.generator.default_chunk()?,
            self.generator.default_light_payload()?,
        ))
    }

    /// Return cached light for `coords`, computing and caching it once if needed.
    pub(crate) fn light_for(&mut self, coords: [i32; 2], neighborhood: &Neighborhood) -> Arc<[u8]> {
        if let Some(cached) = self.light_cache.get(&coords) {
            return cached.clone();
        }
        let fast = self.fast_path();
        let is_default = fast.is_some_and(|(tpl, _)| is_uniform_template(neighborhood, tpl));
        let computed = chunk_light(neighborhood, fast);
        if !is_default {
            self.light_cache.insert(coords, computed.clone());
        }
        computed
    }

    /// Encode full Chunk Data + Update Light payload, using the zero-copy
    /// generator template when the 3x3 neighborhood is untouched.
    pub(crate) fn payload_for(&self, neighborhood: &Neighborhood, light: &Arc<[u8]>) -> Arc<[u8]> {
        if let (Some(tpl), Some(default_payload)) = (
            self.generator.default_chunk(),
            self.generator.default_terrain_payload(),
        ) && is_uniform_template(neighborhood, tpl)
        {
            return default_payload.clone();
        }
        neighborhood[1][1].encode_payload(light)
    }

    /// Return currently cached light if present, otherwise compute without mutating cache.
    pub(crate) fn cached_or_current_light(&mut self, coords: [i32; 2]) -> Arc<[u8]> {
        if let Some(cached) = self.light_cache.get(&coords) {
            return cached.clone();
        }
        let neighborhood = self.neighborhood(coords);
        chunk_light(&neighborhood, self.fast_path())
    }

    /// Recompute light for `coords` after edits and update or evict its cache entry.
    pub(crate) fn recompute_light(&mut self, coords: [i32; 2]) -> Arc<[u8]> {
        let neighborhood = self.neighborhood(coords);
        let fast = self.fast_path();
        let is_default = fast.is_some_and(|(tpl, _)| is_uniform_template(&neighborhood, tpl));
        let computed = chunk_light(&neighborhood, fast);
        if is_default {
            self.light_cache.remove(&coords);
        } else {
            self.light_cache.insert(coords, computed.clone());
        }
        computed
    }

    pub(crate) fn get_block(&mut self, pos: BlockPos) -> i32 {
        let chunk = self.get_or_load(pos.chunk());
        let (lx, y, lz) = pos.local();
        chunk.get_block(lx, y, lz)
    }

    /// Set a block via copy-on-write on the chunk `Arc`. Returns `true` if changed.
    pub(crate) fn set_block(&mut self, pos: BlockPos, state: i32) -> bool {
        let coords = pos.chunk();
        let (lx, y, lz) = pos.local();
        let _ = self.get_or_load(coords);
        let chunk = self.chunks.get_mut(&coords).expect("chunk just loaded");
        if chunk.get_block(lx, y, lz) == state {
            return false;
        }
        let changed = Arc::make_mut(chunk).set_block(lx, y, lz, state);
        if changed {
            if !is_chest_state(state) && !is_furnace_state(state) {
                Arc::make_mut(chunk).remove_block_entity(pos.0);
            }
            self.dirty.insert(coords);
            self.touch(coords);
        }
        changed
    }

    pub(crate) fn chest_items(
        &mut self,
        pos: BlockPos,
    ) -> Option<[Option<crate::game::ItemStack>; super::CHEST_SLOTS]> {
        let coords = pos.chunk();
        let chunk = self.get_or_load(coords);
        let (lx, y, lz) = pos.local();
        if !is_chest_state(chunk.get_block(lx, y, lz)) {
            return None;
        }
        match chunk.block_entity(pos.0) {
            Some(super::BlockEntity::Chest { items }) => Some(**items),
            _ => Some([None; super::CHEST_SLOTS]),
        }
    }

    pub(crate) fn update_chest(
        &mut self,
        pos: BlockPos,
        mutate: impl FnOnce(&mut [Option<crate::game::ItemStack>; super::CHEST_SLOTS]) -> bool,
    ) -> Option<[Option<crate::game::ItemStack>; super::CHEST_SLOTS]> {
        let coords = pos.chunk();
        let _ = self.get_or_load(coords);
        let chunk = self.chunks.get_mut(&coords).expect("chunk just loaded");
        let (lx, y, lz) = pos.local();
        if !is_chest_state(chunk.get_block(lx, y, lz)) {
            return None;
        }
        let mut items = match chunk.block_entity(pos.0) {
            Some(super::BlockEntity::Chest { items }) => **items,
            _ => [None; super::CHEST_SLOTS],
        };
        if mutate(&mut items) {
            Arc::make_mut(chunk).set_block_entity(
                pos.0,
                super::BlockEntity::Chest {
                    items: Box::new(items),
                },
            );
            self.dirty.insert(coords);
            self.touch(coords);
        }
        Some(items)
    }

    pub(crate) fn furnace_data(&mut self, pos: BlockPos) -> Option<super::FurnaceData> {
        let coords = pos.chunk();
        let chunk = self.get_or_load(coords);
        let (lx, y, lz) = pos.local();
        if !is_furnace_state(chunk.get_block(lx, y, lz)) {
            return None;
        }
        match chunk.block_entity(pos.0) {
            Some(super::BlockEntity::Furnace(data)) => Some((**data).clone()),
            _ => Some(super::FurnaceData::default()),
        }
    }

    pub(crate) fn update_furnace(
        &mut self,
        pos: BlockPos,
        mutate: impl FnOnce(&mut super::FurnaceData) -> bool,
    ) -> Option<super::FurnaceData> {
        let coords = pos.chunk();
        let _ = self.get_or_load(coords);
        let chunk = self.chunks.get_mut(&coords).expect("chunk just loaded");
        let (lx, y, lz) = pos.local();
        if !is_furnace_state(chunk.get_block(lx, y, lz)) {
            return None;
        }
        let mut data = match chunk.block_entity(pos.0) {
            Some(super::BlockEntity::Furnace(existing)) => (**existing).clone(),
            _ => super::FurnaceData::default(),
        };
        if mutate(&mut data) {
            Arc::make_mut(chunk)
                .set_block_entity(pos.0, super::BlockEntity::Furnace(Box::new(data.clone())));
            self.dirty.insert(coords);
            self.touch(coords);
        }
        Some(data)
    }

    /// Advance all loaded furnaces by 1 tick (20 TPS).
    /// Returns `(pos, state_changed_to)` for furnaces whose progress or `lit` state changed.
    pub(crate) fn tick_furnaces(&mut self) -> Vec<(BlockPos, Option<i32>)> {
        let mut furnace_positions: Vec<([i32; 2], BlockPos)> = Vec::new();
        for (&coords, chunk) in &self.chunks {
            for (&pos, be) in chunk.block_entities() {
                if matches!(be, super::BlockEntity::Furnace(_)) {
                    furnace_positions.push((coords, BlockPos(pos)));
                }
            }
        }
        let mut updates = Vec::new();
        for (coords, pos) in furnace_positions {
            let Some(chunk) = self.chunks.get_mut(&coords) else {
                continue;
            };
            let (lx, y, lz) = pos.local();
            let current_state = chunk.get_block(lx, y, lz);
            if !is_furnace_state(current_state) {
                continue;
            }
            let Some(super::BlockEntity::Furnace(existing)) = chunk.block_entity(pos.0) else {
                continue;
            };
            let mut data = (**existing).clone();
            let was_lit = data.burn_time > 0;
            let mut changed = false;

            if data.burn_time > 0 {
                data.burn_time -= 1;
                changed = true;
            }

            let recipe = crate::game::match_smelting(data.input);
            let can_smelt = recipe.is_some_and(|(result, _)| match data.output {
                None => true,
                Some(out) => {
                    out.item == result.item
                        && out.damage == result.damage
                        && u16::from(out.count) + u16::from(result.count)
                            <= u16::from(out.definition().max_stack)
                }
            });

            if can_smelt {
                let (result, total) = recipe.unwrap();
                if data.cook_total != total {
                    data.cook_total = total;
                    changed = true;
                }
                if data.burn_time == 0
                    && let Some(mut fuel) = data.fuel
                    && let Some(burn) = crate::game::fuel_burn_ticks(fuel.item)
                {
                    data.burn_time = burn;
                    data.burn_total = burn;
                    if fuel.count > 1 {
                        fuel.count -= 1;
                        data.fuel = Some(fuel);
                    } else {
                        data.fuel = crate::game::container_remainder(fuel.item);
                    }
                    changed = true;
                }
                if data.burn_time > 0 {
                    data.cook_time = data.cook_time.saturating_add(1);
                    changed = true;
                    if data.cook_time >= data.cook_total {
                        data.cook_time = 0;
                        // Consume 1 input item.
                        if let Some(mut inp) = data.input {
                            if inp.count > 1 {
                                inp.count -= 1;
                                data.input = Some(inp);
                            } else {
                                data.input = None;
                            }
                        }
                        // Produce output item.
                        match data.output {
                            None => data.output = Some(result),
                            Some(mut out) => {
                                out.count += result.count;
                                data.output = Some(out);
                            }
                        }
                    }
                } else if data.cook_time > 0 {
                    data.cook_time = data.cook_time.saturating_sub(2);
                    changed = true;
                }
            } else if data.cook_time > 0 {
                data.cook_time = 0;
                changed = true;
            }

            if changed {
                let is_lit = data.burn_time > 0;
                let new_block_state = if was_lit != is_lit {
                    furnace_lit_state(current_state, is_lit)
                } else {
                    None
                };
                Arc::make_mut(chunk)
                    .set_block_entity(pos.0, super::BlockEntity::Furnace(Box::new(data)));
                self.dirty.insert(coords);
                updates.push((pos, new_block_state));
            }
        }
        updates
    }

    pub(crate) fn drain_dirty(&mut self) -> Vec<([i32; 2], Arc<Chunk>)> {
        let coords: Vec<[i32; 2]> = self.dirty.drain().collect();
        coords
            .into_iter()
            .filter_map(|c| self.chunks.get(&c).cloned().map(|chunk| (c, chunk)))
            .collect()
    }

    pub(crate) fn mark_dirty(&mut self, chunks: impl IntoIterator<Item = [i32; 2]>) {
        self.dirty.extend(chunks);
    }
}

pub(crate) fn is_chest_state(state: i32) -> bool {
    crate::game::registry()
        .state_by_id(state)
        .is_some_and(|(_, def, _)| {
            def.name == "minecraft:chest" || def.name == "minecraft:trapped_chest"
        })
}

pub(crate) fn is_furnace_state(state: i32) -> bool {
    crate::game::registry()
        .state_by_id(state)
        .is_some_and(|(_, def, _)| def.name == "minecraft:furnace")
}

fn furnace_lit_state(current: i32, lit: bool) -> Option<i32> {
    let reg = crate::game::registry();
    let (_, def, sdef) = reg.state_by_id(current)?;
    if def.name != "minecraft:furnace" {
        return None;
    }
    let mut props: std::collections::BTreeMap<String, String> = sdef
        .properties
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    props.insert("lit".into(), if lit { "true" } else { "false" }.into());
    reg.resolve_state(&def.name, &props)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::FlatGenerator;

    #[test]
    fn lru_evicts_clean_chunks_but_retains_dirty_chunks() {
        let generator = Arc::new(FlatGenerator::default());
        let mut store = ChunkStore::with_capacity(generator, None, 16);

        // Edit chunk [0, 0] so it becomes dirty.
        assert!(store.set_block(BlockPos([1, 64, 1]), crate::game::GLASS_STATE));

        // Touch 30 other clean chunks to exceed capacity (16).
        for x in 1..=30 {
            let _ = store.get_or_load([x, 0]);
        }

        // Cache stays bounded and never evicts the dirty chunk [0, 0].
        assert!(store.cached_len() <= 16);
        assert_eq!(
            store.get_block(BlockPos([1, 64, 1])),
            crate::game::GLASS_STATE
        );
    }

    #[test]
    fn furnace_smelts_raw_iron_with_coal_and_toggles_lit_state() {
        let generator = Arc::new(FlatGenerator::default());
        let mut store = ChunkStore::new(generator, None);
        let pos = BlockPos([3, 64, 3]);
        let furnace_state = crate::game::block_by_name("furnace").unwrap().default_state;
        store.set_block(pos, furnace_state);

        let raw_iron = crate::game::item_by_name("raw_iron").unwrap().id;
        let coal = crate::game::item_by_name("coal").unwrap().id;
        store.update_furnace(pos, |f| {
            f.input = crate::game::ItemStack::new(raw_iron, 1);
            f.fuel = crate::game::ItemStack::new(coal, 1);
            true
        });

        let mut saw_lit = false;
        for _ in 0..200 {
            for (p, new_state) in store.tick_furnaces() {
                if p == pos
                    && let Some(st) = new_state
                {
                    store.set_block(pos, st);
                    saw_lit = true;
                }
            }
        }
        assert!(saw_lit);
        let data = store.furnace_data(pos).unwrap();
        assert!(data.input.is_none());
        assert!(data.fuel.is_none());
        assert_eq!(
            data.output.unwrap().definition().name,
            "minecraft:iron_ingot"
        );
    }
}
