//! Bounded sparse edits over the flat terrain. The edit lock also serializes
//! delivery to chunk subscribers and reconciliation of just-written snapshots.
use super::*;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
};

pub(crate) const MAX_EDITS: usize = 65_536;
const MAX_CHUNK_EDITS: usize = 256;
const PERSISTED_MAX_CHUNK_EDITS: usize = (HEIGHT as usize) * 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPos(pub [i32; 3]);
impl BlockPos {
    pub(crate) fn chunk(self) -> [i32; 2] {
        [self.0[0].div_euclid(16), self.0[2].div_euclid(16)]
    }
    pub(crate) fn valid(self) -> bool {
        let [x, y, z] = self.0;
        (MIN_Y..MIN_Y + HEIGHT).contains(&y)
            && [x, z].iter().all(|&c| f64::from(c).abs() < WORLD_LIMIT)
    }
    pub(crate) fn on_face(self, face: i32) -> Option<Self> {
        let (axis, offset): (usize, i32) = match face {
            0 => (1, -1),
            1 => (1, 1),
            2 => (2, -1),
            3 => (2, 1),
            4 => (0, -1),
            5 => (0, 1),
            _ => return None,
        };
        let mut next = self.0;
        next[axis] = next[axis].checked_add(offset)?;
        Some(Self(next))
    }
    pub(crate) fn index(self) -> u32 {
        let [x, y, z] = self.0;
        ((y - MIN_Y) * 256 + z.rem_euclid(16) * 16 + x.rem_euclid(16)) as u32
    }
    pub(crate) fn from_index([x, z]: [i32; 2], index: u32) -> Self {
        let i = index as i32;
        Self([x * 16 + i % 16, MIN_Y + i / 256, z * 16 + i / 16 % 16])
    }
}

pub(crate) type Edits = BTreeMap<u32, i32>;
pub(crate) type Snapshot = Option<Arc<Edits>>;
#[derive(Debug)]
pub(crate) struct Blocks {
    generator: Arc<FlatGenerator>,
    chunks: HashMap<[i32; 2], Arc<Edits>>,
    light_cache: HashMap<[i32; 2], Arc<[u8]>>,
    dirty_chunks: HashSet<[i32; 2]>,
    count: usize,
    max_edits: usize,
    max_chunk_edits: usize,
}
impl Default for Blocks {
    fn default() -> Self {
        Self {
            generator: Arc::new(FlatGenerator::default()),
            chunks: HashMap::new(),
            light_cache: HashMap::new(),
            dirty_chunks: HashSet::new(),
            count: 0,
            max_edits: MAX_EDITS,
            max_chunk_edits: MAX_CHUNK_EDITS,
        }
    }
}
#[derive(Debug, Default)]
pub(crate) struct BlockWorld(Mutex<Blocks>);
impl BlockWorld {
    pub(crate) fn with_generator(
        generator: Arc<FlatGenerator>,
        initial_chunks: HashMap<[i32; 2], Edits>,
        persisted: bool,
    ) -> Self {
        let mut count = 0usize;
        let mut chunks = HashMap::with_capacity(initial_chunks.len());
        for (coords, edits) in initial_chunks {
            if !edits.is_empty() {
                count += edits.len();
                chunks.insert(coords, Arc::new(edits));
            }
        }
        Self(Mutex::new(Blocks {
            generator,
            chunks,
            light_cache: HashMap::new(),
            dirty_chunks: HashSet::new(),
            count,
            max_edits: if persisted { usize::MAX } else { MAX_EDITS },
            max_chunk_edits: if persisted {
                PERSISTED_MAX_CHUNK_EDITS
            } else {
                MAX_CHUNK_EDITS
            },
        }))
    }
    pub(crate) fn lock(&self) -> MutexGuard<'_, Blocks> {
        self.0.lock().expect("block world poisoned")
    }
}

impl Blocks {
    pub(crate) fn generator(&self) -> &FlatGenerator {
        &self.generator
    }
    pub(crate) fn snapshot(&self, chunk: [i32; 2]) -> Snapshot {
        self.chunks.get(&chunk).cloned()
    }
    pub(crate) fn neighborhood(&self, [cx, cz]: [i32; 2]) -> Neighborhood {
        std::array::from_fn(|dz| {
            std::array::from_fn(|dx| self.snapshot([cx + dx as i32 - 1, cz + dz as i32 - 1]))
        })
    }

    pub(crate) fn drain_dirty(&mut self) -> Vec<([i32; 2], Option<Edits>)> {
        self.dirty_chunks
            .drain()
            .map(|chunk| (chunk, self.chunks.get(&chunk).map(|e| (**e).clone())))
            .collect()
    }

    pub(crate) fn mark_dirty(&mut self, chunks: impl IntoIterator<Item = [i32; 2]>) {
        self.dirty_chunks.extend(chunks);
    }

    /// Return cached light for `chunk`, computing and caching it once if needed.
    pub(crate) fn light_for(&mut self, chunk: [i32; 2], neighborhood: &Neighborhood) -> Arc<[u8]> {
        if let Some(cached) = self.light_cache.get(&chunk) {
            return cached.clone();
        }
        let computed = chunk_light(neighborhood, &self.generator);
        if has_any_edits(neighborhood) {
            self.light_cache.insert(chunk, computed.clone());
        }
        computed
    }

    /// Return currently cached light if present, otherwise compute without mutating.
    pub(crate) fn cached_or_current_light(&self, chunk: [i32; 2]) -> Arc<[u8]> {
        if let Some(cached) = self.light_cache.get(&chunk) {
            return cached.clone();
        }
        chunk_light(&self.neighborhood(chunk), &self.generator)
    }

    /// Recompute light for `chunk` after edits and update or evict its cache entry.
    pub(crate) fn recompute_light(&mut self, chunk: [i32; 2]) -> Arc<[u8]> {
        let neighborhood = self.neighborhood(chunk);
        let computed = chunk_light(&neighborhood, &self.generator);
        if has_any_edits(&neighborhood) {
            self.light_cache.insert(chunk, computed.clone());
        } else {
            self.light_cache.remove(&chunk);
        }
        computed
    }
    pub(crate) fn get(&self, pos: BlockPos) -> i32 {
        self.chunks
            .get(&pos.chunk())
            .and_then(|s| s.get(&pos.index()))
            .copied()
            .unwrap_or_else(|| self.generator.base_state(pos.0[1]))
    }
    // Caller validates bounds, allowed states, and the current block first.
    pub(crate) fn set(&mut self, pos: BlockPos, state: i32) -> bool {
        let chunk = pos.chunk();
        let index = pos.index();
        if state == self.generator.base_state(pos.0[1]) {
            if let Some(edits) = self.chunks.get_mut(&chunk) {
                if Arc::make_mut(edits).remove(&index).is_some() {
                    self.count -= 1;
                    self.dirty_chunks.insert(chunk);
                }
                if edits.is_empty() {
                    self.chunks.remove(&chunk);
                }
            }
            return true;
        }
        let is_new = !self
            .chunks
            .get(&chunk)
            .is_some_and(|e| e.contains_key(&index));
        if is_new
            && (self.count == self.max_edits
                || self
                    .chunks
                    .get(&chunk)
                    .is_some_and(|e| e.len() == self.max_chunk_edits))
        {
            return false;
        }
        Arc::make_mut(self.chunks.entry(chunk).or_default()).insert(index, state);
        self.count += usize::from(is_new);
        self.dirty_chunks.insert(chunk);
        true
    }
}

pub(super) fn state_at(edits: &Edits, index: u32, generator: &FlatGenerator) -> i32 {
    edits
        .get(&index)
        .copied()
        .unwrap_or_else(|| generator.base_state(MIN_Y + (index / 256) as i32))
}

pub(crate) fn changed_blocks(
    chunk: [i32; 2],
    old: &Snapshot,
    new: &Snapshot,
    generator: &FlatGenerator,
) -> Vec<(BlockPos, i32)> {
    if match (old, new) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    } {
        return Vec::new();
    }
    let empty = Edits::new();
    let a = old.as_deref().unwrap_or(&empty);
    let b = new.as_deref().unwrap_or(&empty);
    let mut changed = Vec::new();
    for i in a.keys().chain(b.keys().filter(|i| !a.contains_key(i))) {
        let state = state_at(b, *i, generator);
        if state_at(a, *i, generator) != state {
            changed.push((BlockPos::from_index(chunk, *i), state));
        }
    }
    changed
}

// Heights are first air above the highest qualifying block, relative to min_y.
fn heights(
    edits: &Edits,
    generator: &FlatGenerator,
    qualifies: impl Fn(i32) -> bool,
) -> [u16; 256] {
    let mut result = [0; 256];
    let top_base = generator.surface_y();
    if top_base >= MIN_Y {
        let base_y = generator.config().base_y;
        for (col, height) in result.iter_mut().enumerate() {
            for y in (base_y..=top_base).rev() {
                if qualifies(state_at(
                    edits,
                    ((y - MIN_Y) as u32) * 256 + col as u32,
                    generator,
                )) {
                    *height = (y - MIN_Y + 1) as u16;
                    break;
                }
            }
        }
    }
    for (&i, &state) in edits {
        if qualifies(state) {
            let height = (i / 256 + 1) as u16;
            result[i as usize % 256] = result[i as usize % 256].max(height);
        }
    }
    result
}

pub(crate) fn chunk_payload_with_light(
    neighborhood: &Neighborhood,
    light: &Arc<[u8]>,
    generator: &FlatGenerator,
) -> Arc<[u8]> {
    let mut out = Vec::with_capacity(45 * 1024);
    let sections = match neighborhood[1][1].as_deref() {
        Some(edits) => {
            let top = heights(edits, generator, |s| s != 0);
            put_heightmaps_with(&mut out, &top);
            edited_sections(edits, generator)
        }
        None => {
            put_heightmaps_with(&mut out, generator.heightmap());
            generator.section_data().to_vec()
        }
    };
    put_varint(sections.len() as i32, &mut out);
    out.extend(sections);
    put_varint(0, &mut out);
    out.extend_from_slice(light);
    out.into()
}

fn edited_sections(edits: &Edits, generator: &FlatGenerator) -> Vec<u8> {
    let biome_id = generator.biome_id();
    let mut sections = Vec::new();
    for section in 0..SECTION_COUNT {
        let offset = (section * 4096) as u32;
        let mut section_edits = edits.range(offset..offset + 4096).peekable();
        if !generator.has_blocks_in_section(section) && section_edits.peek().is_none() {
            sections.extend([0, 0]);
            put_single_palette(0, &mut sections);
            put_single_palette(biome_id, &mut sections);
            continue;
        }
        let mut states = [0i32; 4096];
        if generator.has_blocks_in_section(section) {
            for (y, layer) in states.as_chunks_mut::<256>().0.iter_mut().enumerate() {
                layer.fill(generator.base_state(MIN_Y + (section * 16 + y) as i32));
            }
        }
        for (&i, &state) in section_edits {
            states[(i - offset) as usize] = state;
        }
        sections.extend((states.iter().filter(|&&s| s != 0).count() as u16).to_be_bytes());
        let mut palette = Vec::new();
        let mut indices = [0u64; 4096];
        for (i, state) in states.into_iter().enumerate() {
            indices[i] = palette.iter().position(|&s| s == state).unwrap_or_else(|| {
                palette.push(state);
                palette.len() - 1
            }) as u64;
        }
        if palette.len() == 1 {
            put_single_palette(palette[0], &mut sections);
        } else if palette.len() <= 256 {
            let bits = ((palette.len() - 1).ilog2() as u8 + 1).max(4);
            sections.push(bits);
            put_varint(palette.len() as i32, &mut sections);
            for state in palette {
                put_varint(state, &mut sections);
            }
            pack_entries(&indices, bits, &mut sections);
        } else {
            let direct: [u64; 4096] = std::array::from_fn(|i| states[i] as u64);
            sections.push(15);
            pack_entries(&direct, 15, &mut sections);
        }
        put_single_palette(biome_id, &mut sections);
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_cap_restore_and_snapshot_reconciliation() {
        let mut world = Blocks::default();
        let generator = FlatGenerator::default();
        let pos = BlockPos([-1, 64, 0]);
        assert_eq!(pos.chunk(), [-1, 0]);
        assert!(world.set(pos, 519));
        let old = world.snapshot(pos.chunk());
        assert_eq!(
            heights(old.as_deref().unwrap(), &generator, |s| s != 0)[15],
            129
        );
        assert_eq!(
            heights(old.as_deref().unwrap(), &generator, |s| s != 0 && s != 519)[15],
            128
        );
        assert!(world.set(pos, 0));
        assert!(world.chunks.is_empty());
        assert_eq!(
            changed_blocks(pos.chunk(), &old, &None, &generator),
            vec![(pos, 0)]
        );
        for i in 0..MAX_CHUNK_EDITS as u32 {
            assert!(world.set(BlockPos::from_index([0, 0], i), 1));
        }
        assert!(!world.set(BlockPos::from_index([0, 0], MAX_CHUNK_EDITS as u32), 1));
        assert_eq!(world.count, MAX_CHUNK_EDITS);
    }

    fn decode_light(payload: &[u8]) -> (u64, Vec<&[u8]>, u64, Vec<&[u8]>) {
        let mut r = crate::protocol::Reader::new(payload);
        let read_mask = |r: &mut crate::protocol::Reader<'_>| -> u64 {
            let len = r.varint().unwrap();
            assert!(len <= 1);
            if len == 1 { r.i64().unwrap() as u64 } else { 0 }
        };
        let sky_mask = read_mask(&mut r);
        let block_mask = read_mask(&mut r);
        let _empty_sky = read_mask(&mut r);
        let _empty_block = read_mask(&mut r);
        let sky_count = r.varint().unwrap() as usize;
        let mut sky_arrays = Vec::with_capacity(sky_count);
        for _ in 0..sky_count {
            let len = r.varint().unwrap() as usize;
            sky_arrays.push(r.take(len).unwrap());
        }
        let block_count = r.varint().unwrap() as usize;
        let mut block_arrays = Vec::with_capacity(block_count);
        for _ in 0..block_count {
            let len = r.varint().unwrap() as usize;
            block_arrays.push(r.take(len).unwrap());
        }
        r.finish().unwrap();
        (sky_mask, sky_arrays, block_mask, block_arrays)
    }

    fn light_at(mask: u64, arrays: &[&[u8]], x: usize, y: i32, z: usize) -> u8 {
        let sec = ((y - MIN_Y) / 16 + 1) as u32;
        if (mask & (1u64 << sec)) == 0 {
            return 0;
        }
        let arr_idx = (mask & ((1u64 << sec) - 1)).count_ones() as usize;
        let sample = ((y - MIN_Y) as usize % 16) * 256 + z * 16 + x;
        let byte = arrays[arr_idx][sample / 2];
        if sample.is_multiple_of(2) {
            byte & 0x0f
        } else {
            byte >> 4
        }
    }

    #[test]
    fn broken_bedrock_and_overhangs_receive_skylight_and_wide_palettes_encode() {
        let mut world = Blocks::default();
        // Punch through grass, dirt, and bedrock at (8, 60..=63, 8).
        for y in 60..=63 {
            assert!(world.set(BlockPos([8, y, 8]), 0));
        }
        // Carve a sideways alcove at (9, 61, 8) under intact grass/dirt.
        assert!(world.set(BlockPos([9, 61, 8]), 0));

        let generator = FlatGenerator::default();
        let payload = chunk_light(&world.neighborhood([0, 0]), &generator);
        let (sky_mask, sky_arrays, block_mask, _) = decode_light(&payload);
        assert_eq!(block_mask, 0);
        assert_eq!(light_at(sky_mask, &sky_arrays, 8, 60, 8), 15);
        assert_eq!(light_at(sky_mask, &sky_arrays, 8, 59, 8), 15);
        assert_eq!(light_at(sky_mask, &sky_arrays, 9, 61, 8), 14);

        // >16 distinct block states in one section must encode without error.
        let mut wide = Blocks::default();
        for i in 0..20 {
            assert!(wide.set(BlockPos([i % 16, 64 + i / 16, 0]), i + 1));
        }
        let neighborhood = wide.neighborhood([0, 0]);
        let light = wide.light_for([0, 0], &neighborhood);
        let wide_payload = chunk_payload_with_light(&neighborhood, &light, &generator);
        assert!(!wide_payload.is_empty());
        // Second light_for hits cache (identical Arc pointer).
        assert!(Arc::ptr_eq(&light, &wide.light_for([0, 0], &neighborhood)));
    }

    #[test]
    fn block_light_emits_attenuates_and_crosses_chunk_borders() {
        let mut world = Blocks::default();
        let generator = FlatGenerator::default();
        let glowstone = crate::game::block_by_name("glowstone")
            .unwrap()
            .default_state;
        let torch = crate::game::block_by_name("torch").unwrap().default_state;
        let tinted_glass = crate::game::block_by_name("tinted_glass")
            .unwrap()
            .default_state;

        // Place glowstone (level 15) on the east edge of chunk [0, 0] at (15, 64, 8).
        assert!(world.set(BlockPos([15, 64, 8]), glowstone));
        // Place tinted_glass (opacity 15) on its west side at (14, 64, 8).
        assert!(world.set(BlockPos([14, 64, 8]), tinted_glass));
        // Place torch (level 14) at (2, 64, 2).
        assert!(world.set(BlockPos([2, 64, 2]), torch));

        let c0 = chunk_light(&world.neighborhood([0, 0]), &generator);
        let (_, _, b0_mask, b0_arr) = decode_light(&c0);
        assert_eq!(light_at(b0_mask, &b0_arr, 15, 64, 8), 15);
        assert_eq!(light_at(b0_mask, &b0_arr, 14, 64, 8), 0); // tinted_glass blocks light
        assert_eq!(light_at(b0_mask, &b0_arr, 15, 65, 8), 14);
        assert_eq!(light_at(b0_mask, &b0_arr, 2, 64, 2), 14);
        assert_eq!(light_at(b0_mask, &b0_arr, 3, 64, 2), 13);

        // Neighboring chunk [1, 0] receives cross-border block light at local x=0 (world x=16).
        let c1 = chunk_light(&world.neighborhood([1, 0]), &generator);
        let (_, _, b1_mask, b1_arr) = decode_light(&c1);
        assert_eq!(light_at(b1_mask, &b1_arr, 0, 64, 8), 14);
        assert_eq!(light_at(b1_mask, &b1_arr, 1, 64, 8), 13);
        assert_eq!(light_at(b1_mask, &b1_arr, 13, 64, 8), 1);
        assert_eq!(light_at(b1_mask, &b1_arr, 14, 64, 8), 0);
    }
}
