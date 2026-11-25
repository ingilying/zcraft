//! Bounded sparse edits over the flat terrain. The edit lock also serializes
//! delivery to chunk subscribers and reconciliation of just-written snapshots.
use super::*;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard},
};

pub(crate) const MAX_EDITS: usize = 65_536;
const MAX_CHUNK_EDITS: usize = 256;

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
    fn index(self) -> u32 {
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
#[derive(Debug, Default)]
pub(crate) struct Blocks {
    chunks: HashMap<[i32; 2], Arc<Edits>>,
    count: usize,
}
#[derive(Debug, Default)]
pub(crate) struct BlockWorld(Mutex<Blocks>);
impl BlockWorld {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Blocks> {
        self.0.lock().expect("block world poisoned")
    }
}

pub(super) fn base(y: i32) -> i32 {
    match y {
        60 => crate::game::BEDROCK_STATE,
        61 | 62 => crate::game::DIRT_STATE,
        63 => crate::game::GRASS_BLOCK_STATE,
        _ => crate::game::AIR_STATE,
    }
}

impl Blocks {
    pub(crate) fn snapshot(&self, chunk: [i32; 2]) -> Snapshot {
        self.chunks.get(&chunk).cloned()
    }
    pub(crate) fn neighborhood(&self, [cx, cz]: [i32; 2]) -> Neighborhood {
        std::array::from_fn(|dz| {
            std::array::from_fn(|dx| self.snapshot([cx + dx as i32 - 1, cz + dz as i32 - 1]))
        })
    }
    pub(crate) fn get(&self, pos: BlockPos) -> i32 {
        self.chunks
            .get(&pos.chunk())
            .and_then(|s| s.get(&pos.index()))
            .copied()
            .unwrap_or_else(|| base(pos.0[1]))
    }
    // Caller validates bounds, allowed states, and the current block first.
    pub(crate) fn set(&mut self, pos: BlockPos, state: i32) -> bool {
        let chunk = pos.chunk();
        let index = pos.index();
        if state == base(pos.0[1]) {
            if let Some(edits) = self.chunks.get_mut(&chunk) {
                if Arc::make_mut(edits).remove(&index).is_some() {
                    self.count -= 1;
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
            && (self.count == MAX_EDITS
                || self
                    .chunks
                    .get(&chunk)
                    .is_some_and(|e| e.len() == MAX_CHUNK_EDITS))
        {
            return false;
        }
        Arc::make_mut(self.chunks.entry(chunk).or_default()).insert(index, state);
        self.count += usize::from(is_new);
        true
    }
}

pub(super) fn state_at(edits: &Edits, index: u32) -> i32 {
    edits
        .get(&index)
        .copied()
        .unwrap_or_else(|| base(MIN_Y + (index / 256) as i32))
}

pub(crate) fn changed_blocks(
    chunk: [i32; 2],
    old: &Snapshot,
    new: &Snapshot,
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
        let state = state_at(b, *i);
        if state_at(a, *i) != state {
            changed.push((BlockPos::from_index(chunk, *i), state));
        }
    }
    changed
}

// Heights are first air above the highest qualifying block, relative to min_y.
fn heights(edits: &Edits, qualifies: impl Fn(i32) -> bool) -> [u16; 256] {
    let mut result = [0; 256];
    for (col, height) in result.iter_mut().enumerate() {
        for y in (60..=SURFACE_Y).rev() {
            if qualifies(state_at(edits, ((y - MIN_Y) as u32) * 256 + col as u32)) {
                *height = (y - MIN_Y + 1) as u16;
                break;
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

pub(crate) fn chunk_payload_with_light(neighborhood: &Neighborhood) -> Arc<[u8]> {
    let mut out = Vec::with_capacity(45 * 1024);
    let sections = match neighborhood[1][1].as_deref() {
        Some(edits) => {
            let top = heights(edits, |s| s != 0);
            put_heightmaps_with(&mut out, &top);
            edited_sections(edits)
        }
        None => {
            put_heightmaps(&mut out);
            section_data()
        }
    };
    put_varint(sections.len() as i32, &mut out);
    out.extend(sections);
    put_varint(0, &mut out);
    out.extend_from_slice(&chunk_light(neighborhood));
    out.into()
}

fn edited_sections(edits: &Edits) -> Vec<u8> {
    let mut sections = Vec::new();
    for section in 0..SECTION_COUNT {
        let offset = (section * 4096) as u32;
        let mut section_edits = edits.range(offset..offset + 4096).peekable();
        if section != GROUND_SECTION && section_edits.peek().is_none() {
            sections.extend([0, 0]);
            put_single_palette(0, &mut sections);
            put_single_palette(PLAINS_BIOME_ID, &mut sections);
            continue;
        }
        let mut states = [0i32; 4096];
        if section == GROUND_SECTION {
            for (y, layer) in states.as_chunks_mut::<256>().0.iter_mut().enumerate() {
                layer.fill(base(MIN_Y + (section * 16 + y) as i32));
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
        put_single_palette(PLAINS_BIOME_ID, &mut sections);
    }
    sections
}

fn pack_entries(entries: &[u64; 4096], bits: u8, out: &mut Vec<u8>) {
    let per_long = 64 / usize::from(bits);
    let longs = entries.len().div_ceil(per_long);
    put_varint(longs as i32, out);
    for chunk in entries.chunks(per_long) {
        let mut value = 0u64;
        for (i, &entry) in chunk.iter().enumerate() {
            value |= entry << (i * usize::from(bits));
        }
        out.extend(value.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_cap_restore_and_snapshot_reconciliation() {
        let mut world = Blocks::default();
        let pos = BlockPos([-1, 64, 0]);
        assert_eq!(pos.chunk(), [-1, 0]);
        assert!(world.set(pos, 519));
        let old = world.snapshot(pos.chunk());
        assert_eq!(heights(old.as_deref().unwrap(), |s| s != 0)[15], 129);
        assert_eq!(
            heights(old.as_deref().unwrap(), |s| s != 0 && s != 519)[15],
            128
        );
        assert!(world.set(pos, 0));
        assert!(world.chunks.is_empty());
        assert_eq!(changed_blocks(pos.chunk(), &old, &None), vec![(pos, 0)]);
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

        let payload = chunk_light(&world.neighborhood([0, 0]));
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
        let wide_payload = chunk_payload_with_light(&wide.neighborhood([0, 0]));
        assert!(!wide_payload.is_empty());
    }

    #[test]
    fn block_light_emits_attenuates_and_crosses_chunk_borders() {
        let mut world = Blocks::default();
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

        let c0 = chunk_light(&world.neighborhood([0, 0]));
        let (_, _, b0_mask, b0_arr) = decode_light(&c0);
        assert_eq!(light_at(b0_mask, &b0_arr, 15, 64, 8), 15);
        assert_eq!(light_at(b0_mask, &b0_arr, 14, 64, 8), 0); // tinted_glass blocks light
        assert_eq!(light_at(b0_mask, &b0_arr, 15, 65, 8), 14);
        assert_eq!(light_at(b0_mask, &b0_arr, 2, 64, 2), 14);
        assert_eq!(light_at(b0_mask, &b0_arr, 3, 64, 2), 13);

        // Neighboring chunk [1, 0] receives cross-border block light at local x=0 (world x=16).
        let c1 = chunk_light(&world.neighborhood([1, 0]));
        let (_, _, b1_mask, b1_arr) = decode_light(&c1);
        assert_eq!(light_at(b1_mask, &b1_arr, 0, 64, 8), 14);
        assert_eq!(light_at(b1_mask, &b1_arr, 1, 64, 8), 13);
        assert_eq!(light_at(b1_mask, &b1_arr, 13, 64, 8), 1);
        assert_eq!(light_at(b1_mask, &b1_arr, 14, 64, 8), 0);
    }
}
