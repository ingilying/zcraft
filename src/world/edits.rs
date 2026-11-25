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

fn base(y: i32) -> i32 {
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

fn state_at(edits: &Edits, index: u32) -> i32 {
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
    let mut result = [128; 256];
    // The permanent bedrock layer bounds the fallback search.
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

pub(crate) fn sky_heights(edits: &Snapshot) -> [u16; 256] {
    match edits {
        Some(edits) => heights(edits, |state| {
            crate::game::block_by_state(state).is_some_and(|block| !block.skylight)
        }),
        None => [(SURFACE_Y + 1 - MIN_Y) as u16; 256],
    }
}

pub(crate) fn light_payload(top: &[u16; 256]) -> Arc<[u8]> {
    let min = usize::from(*top.iter().min().unwrap());
    let max = usize::from(*top.iter().max().unwrap());
    let mut sky = 0u64;
    let mut arrays = Vec::new();
    for section in 1..LIGHT_SECTION_COUNT {
        let bottom = (section - 1) * 16;
        if bottom + 16 <= min {
            continue;
        }
        sky |= 1 << section;
        let start = arrays.len();
        put_varint(LIGHT_BYTES as i32, &mut arrays);
        if bottom >= max {
            arrays.resize(arrays.len() + LIGHT_BYTES, 0xff);
        } else {
            arrays.reserve(LIGHT_BYTES);
            for y in bottom..bottom + 16 {
                for pair in top.as_chunks::<2>().0 {
                    arrays.push(
                        if y >= usize::from(pair[0]) { 0x0f } else { 0 }
                            | if y >= usize::from(pair[1]) { 0xf0 } else { 0 },
                    );
                }
            }
        }
        debug_assert_eq!(arrays.len() - start, LIGHT_BYTES + 2);
    }
    let mut out = Vec::with_capacity(arrays.len() + 32);
    put_bitset(sky, &mut out);
    put_bitset(0, &mut out);
    put_bitset(ALL_LIGHT_SECTIONS ^ sky, &mut out);
    put_bitset(ALL_LIGHT_SECTIONS, &mut out);
    put_varint(sky.count_ones() as i32, &mut out);
    out.extend(arrays);
    put_varint(0, &mut out);
    out.into()
}

pub(crate) fn edited_payload(edits: &Arc<Edits>) -> Arc<[u8]> {
    let top = heights(edits, |s| s != 0);
    let mut out = Vec::with_capacity(45 * 1024);
    put_heightmaps_with(&mut out, &top);
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
        } else {
            // The supported terrain and edit whitelist have at most six states.
            sections.push(4);
            put_varint(palette.len() as i32, &mut sections);
            for state in palette {
                put_varint(state, &mut sections);
            }
            put_varint(256, &mut sections);
            for row in indices.as_chunks::<16>().0 {
                let value = row
                    .iter()
                    .enumerate()
                    .fold(0, |n, (i, s)| n | (s << (4 * i)));
                sections.extend(value.to_be_bytes());
            }
        }
        put_single_palette(PLAINS_BIOME_ID, &mut sections);
    }
    put_varint(sections.len() as i32, &mut out);
    out.extend(sections);
    put_varint(0, &mut out);
    out.extend_from_slice(&light_payload(&heights(edits, |state| {
        crate::game::block_by_state(state).is_some_and(|block| !block.skylight)
    })));
    out.into()
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
}
