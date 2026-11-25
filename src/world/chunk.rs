//! Section-based chunk storage model (Java 1.21.1 / protocol 767).
//!
//! Each chunk owns 24 vertical 16x16x16 sections (`y = -64..320`). Uniform
//! sections (such as empty air) use `BlockStorage::Single` with zero heap
//! allocation, promoting to `Dense` on demand and collapsing back when uniform.
use super::{
    HEIGHT, MIN_Y, PLAINS_BIOME_ID, SECTION_COUNT, WORLD_LIMIT, pack_entries, put_heightmaps_with,
    put_single_palette, put_varint,
};
use std::sync::Arc;

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

    #[inline]
    pub(crate) fn local(self) -> (usize, i32, usize) {
        (
            self.0[0].rem_euclid(16) as usize,
            self.0[1],
            self.0[2].rem_euclid(16) as usize,
        )
    }
}

/// Block storage for one 16x16x16 section, indexed as `ly * 256 + lz * 16 + lx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockStorage {
    Single(i32),
    Dense(Box<[i32; 4096]>),
}

/// Biome storage for one 16x16x16 section (4x4x4 = 64 biome cells).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BiomeStorage {
    Single(i32),
    Dense(Box<[i32; 64]>),
}

#[inline]
fn is_emissive(state: i32) -> bool {
    crate::game::block_by_state(state).is_some_and(|def| def.light_emission > 0)
}

/// One 16x16x16 chunk section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSection {
    blocks: BlockStorage,
    non_air: u16,
    emissive: u16,
    biomes: BiomeStorage,
}

impl Default for ChunkSection {
    fn default() -> Self {
        Self::empty(PLAINS_BIOME_ID)
    }
}

impl ChunkSection {
    pub fn empty(biome_id: i32) -> Self {
        Self::filled(crate::game::AIR_STATE, biome_id)
    }

    pub fn filled(state: i32, biome_id: i32) -> Self {
        let non_air = if state == crate::game::AIR_STATE {
            0
        } else {
            4096
        };
        let emissive = if is_emissive(state) { 4096 } else { 0 };
        Self {
            blocks: BlockStorage::Single(state),
            non_air,
            emissive,
            biomes: BiomeStorage::Single(biome_id),
        }
    }

    pub fn from_dense(blocks: Box<[i32; 4096]>, biomes: BiomeStorage) -> Self {
        let first = blocks[0];
        if blocks.iter().all(|&s| s == first) {
            let non_air = if first == crate::game::AIR_STATE {
                0
            } else {
                4096
            };
            let emissive = if is_emissive(first) { 4096 } else { 0 };
            return Self {
                blocks: BlockStorage::Single(first),
                non_air,
                emissive,
                biomes,
            };
        }
        let mut non_air = 0u16;
        let mut emissive = 0u16;
        for &s in blocks.iter() {
            if s != crate::game::AIR_STATE {
                non_air += 1;
                if is_emissive(s) {
                    emissive += 1;
                }
            }
        }
        Self {
            blocks: BlockStorage::Dense(blocks),
            non_air,
            emissive,
            biomes,
        }
    }

    #[inline]
    pub fn blocks(&self) -> &BlockStorage {
        &self.blocks
    }

    #[inline]
    pub fn biomes(&self) -> &BiomeStorage {
        &self.biomes
    }

    #[inline]
    pub fn non_air_count(&self) -> u16 {
        self.non_air
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.non_air == 0
    }

    #[inline]
    pub fn has_emissive(&self) -> bool {
        self.emissive > 0
    }

    #[inline]
    pub fn get(&self, lx: usize, ly: usize, lz: usize) -> i32 {
        match &self.blocks {
            BlockStorage::Single(state) => *state,
            BlockStorage::Dense(arr) => arr[ly * 256 + lz * 16 + lx],
        }
    }

    /// Set a single block in this section. Returns `true` if the state changed.
    pub fn set(&mut self, lx: usize, ly: usize, lz: usize, state: i32) -> bool {
        let idx = ly * 256 + lz * 16 + lx;
        let prev = match &mut self.blocks {
            BlockStorage::Single(current) => {
                if *current == state {
                    return false;
                }
                let prev = *current;
                let mut arr = Box::new([prev; 4096]);
                arr[idx] = state;
                self.blocks = BlockStorage::Dense(arr);
                prev
            }
            BlockStorage::Dense(arr) => {
                let prev = arr[idx];
                if prev == state {
                    return false;
                }
                arr[idx] = state;
                prev
            }
        };
        if prev == crate::game::AIR_STATE && state != crate::game::AIR_STATE {
            self.non_air += 1;
        } else if prev != crate::game::AIR_STATE && state == crate::game::AIR_STATE {
            self.non_air -= 1;
        }
        let prev_emissive = is_emissive(prev);
        let next_emissive = is_emissive(state);
        if !prev_emissive && next_emissive {
            self.emissive += 1;
        } else if prev_emissive && !next_emissive {
            self.emissive -= 1;
        }
        if self.non_air == 0 {
            self.blocks = BlockStorage::Single(crate::game::AIR_STATE);
        }
        true
    }

    pub fn set_biome(&mut self, bx: usize, by: usize, bz: usize, biome_id: i32) {
        let idx = by * 16 + bz * 4 + bx;
        match &mut self.biomes {
            BiomeStorage::Single(current) => {
                if *current != biome_id {
                    let mut arr = Box::new([*current; 64]);
                    arr[idx] = biome_id;
                    self.biomes = BiomeStorage::Dense(arr);
                }
            }
            BiomeStorage::Dense(arr) => {
                arr[idx] = biome_id;
            }
        }
    }

    pub fn fill_layer(&mut self, ly: usize, state: i32) {
        for lz in 0..16 {
            for lx in 0..16 {
                self.set(lx, ly, lz, state);
            }
        }
    }

    pub fn encode_wire(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.non_air.to_be_bytes());
        match &self.blocks {
            BlockStorage::Single(state) => put_single_palette(*state, out),
            BlockStorage::Dense(states) => {
                let mut palette = Vec::new();
                let mut indices = [0u64; 4096];
                for (i, &state) in states.iter().enumerate() {
                    indices[i] = palette.iter().position(|&s| s == state).unwrap_or_else(|| {
                        palette.push(state);
                        palette.len() - 1
                    }) as u64;
                }
                if palette.len() == 1 {
                    put_single_palette(palette[0], out);
                } else if palette.len() <= 256 {
                    let bits = ((palette.len() - 1).ilog2() as u8 + 1).max(4);
                    out.push(bits);
                    put_varint(palette.len() as i32, out);
                    for state in palette {
                        put_varint(state, out);
                    }
                    pack_entries(&indices, bits, out);
                } else {
                    let direct: [u64; 4096] = std::array::from_fn(|i| states[i] as u64);
                    out.push(15);
                    pack_entries(&direct, 15, out);
                }
            }
        }
        encode_biomes_wire(&self.biomes, out);
    }
}

fn encode_biomes_wire(biomes: &BiomeStorage, out: &mut Vec<u8>) {
    match biomes {
        BiomeStorage::Single(id) => put_single_palette(*id, out),
        BiomeStorage::Dense(cells) => {
            let mut palette = Vec::new();
            let mut indices = [0u64; 64];
            for (i, &id) in cells.iter().enumerate() {
                indices[i] = palette.iter().position(|&b| b == id).unwrap_or_else(|| {
                    palette.push(id);
                    palette.len() - 1
                }) as u64;
            }
            if palette.len() == 1 {
                put_single_palette(palette[0], out);
            } else if palette.len() <= 8 {
                let bits = ((palette.len() - 1).ilog2() as u8 + 1).max(1);
                out.push(bits);
                put_varint(palette.len() as i32, out);
                for id in palette {
                    put_varint(id, out);
                }
                pack_small_entries(&indices, bits, out);
            } else {
                let direct: [u64; 64] = std::array::from_fn(|i| cells[i] as u64);
                out.push(6);
                pack_small_entries(&direct, 6, out);
            }
        }
    }
}

fn pack_small_entries(entries: &[u64; 64], bits: u8, out: &mut Vec<u8>) {
    let per_long = 64 / usize::from(bits);
    let longs = entries.len().div_ceil(per_long);
    put_varint(longs as i32, out);
    for chunk in entries.chunks(per_long) {
        let mut value = 0u64;
        for (i, &entry) in chunk.iter().enumerate() {
            value |= entry << (i * usize::from(bits));
        }
        out.extend_from_slice(&value.to_be_bytes());
    }
}

/// Complete 16x384x16 chunk composed of 24 sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    sections: Box<[ChunkSection; SECTION_COUNT]>,
}

impl Chunk {
    pub fn empty(biome_id: i32) -> Self {
        Self {
            sections: Box::new(std::array::from_fn(|_| ChunkSection::empty(biome_id))),
        }
    }

    pub fn from_sections(sections: Box<[ChunkSection; SECTION_COUNT]>) -> Self {
        Self { sections }
    }

    #[inline]
    pub fn sections(&self) -> &[ChunkSection; SECTION_COUNT] {
        &self.sections
    }

    #[inline]
    pub fn section(&self, index: usize) -> &ChunkSection {
        &self.sections[index]
    }

    #[inline]
    pub fn section_mut(&mut self, index: usize) -> &mut ChunkSection {
        &mut self.sections[index]
    }

    #[inline]
    pub fn get_block(&self, lx: usize, y: i32, lz: usize) -> i32 {
        let y_rel = y - MIN_Y;
        if !(0..HEIGHT).contains(&y_rel) {
            return crate::game::AIR_STATE;
        }
        let sec = (y_rel / 16) as usize;
        let ly = (y_rel % 16) as usize;
        self.sections[sec].get(lx, ly, lz)
    }

    #[inline]
    pub fn set_block(&mut self, lx: usize, y: i32, lz: usize, state: i32) -> bool {
        let y_rel = y - MIN_Y;
        if !(0..HEIGHT).contains(&y_rel) {
            return false;
        }
        let sec = (y_rel / 16) as usize;
        let ly = (y_rel % 16) as usize;
        self.sections[sec].set(lx, ly, lz, state)
    }

    /// Vertical Y-offset range `(min_y_rel, max_y_rel)` spanning all sections
    /// that contain at least one light-emitting block, or `None` if none exist.
    pub fn emissive_y_bounds(&self) -> Option<(usize, usize)> {
        let first = self.sections.iter().position(ChunkSection::has_emissive)?;
        let last = self
            .sections
            .iter()
            .rposition(ChunkSection::has_emissive)
            .expect("first exists");
        Some((first * 16, (last + 1) * 16))
    }

    /// Heightmap: first air Y relative to `MIN_Y` above the highest qualifying block.
    pub fn heightmap(&self, qualifies: impl Fn(i32) -> bool) -> [u16; 256] {
        let mut result = [0u16; 256];
        let mut remaining = 256usize;
        for sec_idx in (0..SECTION_COUNT).rev() {
            if remaining == 0 {
                break;
            }
            let sec = &self.sections[sec_idx];
            match sec.blocks() {
                BlockStorage::Single(state) => {
                    if !qualifies(*state) {
                        continue;
                    }
                    let top = ((sec_idx + 1) * 16) as u16;
                    for slot in &mut result {
                        if *slot == 0 {
                            *slot = top;
                            remaining -= 1;
                        }
                    }
                }
                BlockStorage::Dense(arr) => {
                    for ly in (0..16).rev() {
                        if remaining == 0 {
                            break;
                        }
                        let row_base = ly * 256;
                        let height = (sec_idx * 16 + ly + 1) as u16;
                        for (col, slot) in result.iter_mut().enumerate() {
                            if *slot == 0 && qualifies(arr[row_base + col]) {
                                *slot = height;
                                remaining -= 1;
                            }
                        }
                    }
                }
            }
        }
        result
    }

    /// Encode all 24 sections into protocol 767 wire bytes.
    pub fn encode_sections(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2300);
        for sec in self.sections.iter() {
            sec.encode_wire(&mut out);
        }
        out
    }

    /// Encode full Chunk Data + Update Light payload (without x/z header).
    pub fn encode_payload(&self, light: &[u8]) -> Arc<[u8]> {
        let mut out = Vec::with_capacity(45 * 1024);
        let top = self.heightmap(|s| s != crate::game::AIR_STATE);
        put_heightmaps_with(&mut out, &top);
        let sections = self.encode_sections();
        put_varint(sections.len() as i32, &mut out);
        out.extend_from_slice(&sections);
        put_varint(0, &mut out); // Block entity count.
        out.extend_from_slice(light);
        out.into()
    }

    /// Compute block differences between `old` and `self` for reconciliation.
    pub fn diff_blocks(&self, [cx, cz]: [i32; 2], old: &Self) -> Vec<(BlockPos, i32)> {
        let mut changes = Vec::new();
        for sec_idx in 0..SECTION_COUNT {
            let a = &old.sections[sec_idx];
            let b = &self.sections[sec_idx];
            if a.blocks == b.blocks {
                continue;
            }
            let base_y = MIN_Y + (sec_idx * 16) as i32;
            for ly in 0..16 {
                for lz in 0..16 {
                    for lx in 0..16 {
                        let next = b.get(lx, ly, lz);
                        if a.get(lx, ly, lz) != next {
                            changes.push((
                                BlockPos([
                                    cx * 16 + lx as i32,
                                    base_y + ly as i32,
                                    cz * 16 + lz as i32,
                                ]),
                                next,
                            ));
                        }
                    }
                }
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_promotes_to_dense_and_collapses_back_to_single_air() {
        let mut sec = ChunkSection::empty(PLAINS_BIOME_ID);
        assert!(matches!(sec.blocks(), BlockStorage::Single(0)));
        assert_eq!(sec.non_air_count(), 0);

        assert!(sec.set(3, 4, 5, crate::game::STONE_STATE));
        assert!(matches!(sec.blocks(), BlockStorage::Dense(_)));
        assert_eq!(sec.non_air_count(), 1);
        assert_eq!(sec.get(3, 4, 5), crate::game::STONE_STATE);
        assert!(!sec.set(3, 4, 5, crate::game::STONE_STATE));

        assert!(sec.set(3, 4, 5, crate::game::AIR_STATE));
        assert!(matches!(sec.blocks(), BlockStorage::Single(0)));
        assert_eq!(sec.non_air_count(), 0);
    }

    #[test]
    fn chunk_heightmap_and_diff_blocks_are_exact() {
        let mut a = Chunk::empty(PLAINS_BIOME_ID);
        a.set_block(1, 63, 2, crate::game::GRASS_BLOCK_STATE);
        let mut b = a.clone();
        b.set_block(1, 64, 2, crate::game::GLASS_STATE);
        b.set_block(1, 63, 2, crate::game::AIR_STATE);

        let top = b.heightmap(|s| s != 0);
        assert_eq!(top[2 * 16 + 1], (64 - MIN_Y + 1) as u16);
        assert_eq!(top[0], 0);

        let diff = b.diff_blocks([-1, 2], &a);
        assert_eq!(
            diff,
            vec![
                (BlockPos([-15, 63, 34]), crate::game::AIR_STATE),
                (BlockPos([-15, 64, 34]), crate::game::GLASS_STATE),
            ]
        );
    }
}
