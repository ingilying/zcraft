//! Vanilla-aligned sky light and block light propagation over a 3x3 `Chunk`
//! neighborhood (Java 1.21.1 / protocol 767), independent of terrain generator.
use super::{
    ALL_LIGHT_SECTIONS, BlockPos, Chunk, HEIGHT, LIGHT_BYTES, LIGHT_SECTION_COUNT, MIN_Y,
    put_bitset, put_varint,
};
use std::{collections::VecDeque, sync::Arc};

/// 3x3 chunk snapshots ordered as `[dz + 1][dx + 1]` around the center chunk.
pub(crate) type Neighborhood = [[Arc<Chunk>; 3]; 3];

const GRID: usize = 48;
const LAYER: usize = GRID * GRID;

#[inline]
fn light_props(state: i32) -> (bool, u8, u8) {
    crate::game::block_by_state(state).map_or((false, 0, 15), |def| {
        (
            def.skylight,
            def.light_emission.min(15),
            def.light_block.min(15),
        )
    })
}

#[inline]
fn allows_direct_skylight(state: i32) -> bool {
    let (skylight, _, atten) = light_props(state);
    skylight && atten == 0
}

pub(crate) fn is_uniform_template(neighborhood: &Neighborhood, template: &Arc<Chunk>) -> bool {
    neighborhood
        .iter()
        .flatten()
        .all(|chunk| Arc::ptr_eq(chunk, template))
}

/// Compute protocol 767 sky-light and block-light payload for the center chunk
/// of `neighborhood`, propagating across chunk borders up to 14 blocks.
pub(crate) fn chunk_light(
    neighborhood: &Neighborhood,
    fast_path: Option<(&Arc<Chunk>, &Arc<[u8]>)>,
) -> Arc<[u8]> {
    if let Some((template, default_light)) = fast_path
        && is_uniform_template(neighborhood, template)
    {
        return default_light.clone();
    }

    let mut sky_top = [0u16; LAYER];
    let mut min_emissive = usize::MAX;
    let mut max_emissive = 0usize;

    for (dz, row) in neighborhood.iter().enumerate() {
        for (dx, chunk) in row.iter().enumerate() {
            if let Some((lo, hi)) = chunk.emissive_y_bounds() {
                min_emissive = min_emissive.min(lo);
                max_emissive = max_emissive.max(hi);
            }
            let chunk_top = chunk.heightmap(|s| !allows_direct_skylight(s));
            for lz in 0..16 {
                let gz = dz * 16 + lz;
                for lx in 0..16 {
                    let gx = dx * 16 + lx;
                    sky_top[gz * GRID + gx] = chunk_top[lz * 16 + lx];
                }
            }
        }
    }

    let min_top = usize::from(*sky_top.iter().min().unwrap_or(&0));
    let max_top = usize::from(*sky_top.iter().max().unwrap_or(&0));
    let band_start = min_top.min(min_emissive).saturating_sub(15) & !15;
    let band_end = max_top
        .max(max_emissive.saturating_add(16))
        .next_multiple_of(16)
        .min(HEIGHT as usize);
    let band_height = band_end.saturating_sub(band_start);

    let cells = band_height * LAYER;
    let mut atten = vec![0u8; cells];
    let mut sky = vec![0u8; cells];
    let mut block = vec![0u8; cells];
    let mut block_buckets: [Vec<usize>; 16] = std::array::from_fn(|_| Vec::new());

    for dy in 0..band_height {
        let y_rel = band_start + dy;
        let world_y = MIN_Y + y_rel as i32;
        let layer_base = dy * LAYER;
        for (dz, row) in neighborhood.iter().enumerate() {
            for (dx, chunk) in row.iter().enumerate() {
                for lz in 0..16 {
                    let gz = dz * 16 + lz;
                    let row_offset = layer_base + gz * GRID + dx * 16;
                    let top_offset = gz * GRID + dx * 16;
                    for lx in 0..16 {
                        let idx = row_offset + lx;
                        let state = chunk.get_block(lx, world_y, lz);
                        let (_, emission, opacity) = light_props(state);
                        atten[idx] = opacity;
                        if y_rel >= usize::from(sky_top[top_offset + lx]) {
                            sky[idx] = 15;
                        }
                        if emission > 0 {
                            block[idx] = emission;
                            if emission >= 2 {
                                block_buckets[usize::from(emission)].push(idx);
                            }
                        }
                    }
                }
            }
        }
    }

    // Seed sky-light BFS from directly lit cells adjacent to darker passable cells.
    let mut sky_buckets: [Vec<usize>; 16] = std::array::from_fn(|_| Vec::new());
    for dy in 0..band_height {
        let base_idx = dy * LAYER;
        for gz in 0..GRID {
            let row_idx = base_idx + gz * GRID;
            for gx in 0..GRID {
                let idx = row_idx + gx;
                if sky[idx] != 15 {
                    continue;
                }
                let needs_spread = (gx > 0 && can_improve(15, atten[idx - 1], sky[idx - 1]))
                    || (gx + 1 < GRID && can_improve(15, atten[idx + 1], sky[idx + 1]))
                    || (gz > 0 && can_improve(15, atten[idx - GRID], sky[idx - GRID]))
                    || (gz + 1 < GRID && can_improve(15, atten[idx + GRID], sky[idx + GRID]))
                    || (dy > 0 && can_improve(15, atten[idx - LAYER], sky[idx - LAYER]))
                    || (dy + 1 < band_height
                        && can_improve(15, atten[idx + LAYER], sky[idx + LAYER]));
                if needs_spread {
                    sky_buckets[15].push(idx);
                }
            }
        }
    }

    propagate(&mut sky, &atten, &mut sky_buckets, band_height);
    propagate(&mut block, &atten, &mut block_buckets, band_height);

    encode_center_light(&sky, &block, band_start, band_end)
}

#[inline]
fn can_improve(level: u8, neighbor_atten: u8, neighbor_light: u8) -> bool {
    level.saturating_sub(neighbor_atten.max(1)) > neighbor_light
}

fn propagate(light: &mut [u8], atten: &[u8], buckets: &mut [Vec<usize>; 16], band_height: usize) {
    let mut queue = VecDeque::new();
    for level in (2..=15u8).rev() {
        queue.extend(buckets[usize::from(level)].drain(..));
        while let Some(idx) = queue.pop_front() {
            if light[idx] != level {
                continue;
            }
            let dy = idx / LAYER;
            let rem = idx % LAYER;
            let gz = rem / GRID;
            let gx = rem % GRID;
            let mut visit = |n: usize| {
                let next = level.saturating_sub(atten[n].max(1));
                if next > light[n] {
                    light[n] = next;
                    if next >= 2 {
                        buckets[usize::from(next)].push(n);
                    }
                }
            };
            if gx > 0 {
                visit(idx - 1);
            }
            if gx + 1 < GRID {
                visit(idx + 1);
            }
            if gz > 0 {
                visit(idx - GRID);
            }
            if gz + 1 < GRID {
                visit(idx + GRID);
            }
            if dy > 0 {
                visit(idx - LAYER);
            }
            if dy + 1 < band_height {
                visit(idx + LAYER);
            }
        }
    }
}

fn encode_center_light(sky: &[u8], block: &[u8], band_start: usize, band_end: usize) -> Arc<[u8]> {
    let mut sky_mask = 0u64;
    let mut block_mask = 0u64;
    let mut sky_arrays = Vec::new();
    let mut block_arrays = Vec::new();

    for section in 1..LIGHT_SECTION_COUNT {
        let bottom = (section - 1) * 16;
        if bottom >= band_end {
            sky_mask |= 1 << section;
            put_varint(LIGHT_BYTES as i32, &mut sky_arrays);
            sky_arrays.resize(sky_arrays.len() + LIGHT_BYTES, 0xff);
            continue;
        }
        if bottom + 16 <= band_start {
            continue;
        }
        if pack_center_section(sky, bottom - band_start, &mut sky_arrays) {
            sky_mask |= 1 << section;
        }
        if pack_center_section(block, bottom - band_start, &mut block_arrays) {
            block_mask |= 1 << section;
        }
    }

    let mut out = Vec::with_capacity(sky_arrays.len() + block_arrays.len() + 32);
    put_bitset(sky_mask, &mut out);
    put_bitset(block_mask, &mut out);
    put_bitset(ALL_LIGHT_SECTIONS ^ sky_mask, &mut out);
    put_bitset(ALL_LIGHT_SECTIONS ^ block_mask, &mut out);
    put_varint(sky_mask.count_ones() as i32, &mut out);
    out.extend(sky_arrays);
    put_varint(block_mask.count_ones() as i32, &mut out);
    out.extend(block_arrays);
    out.into()
}

fn pack_center_section(grid: &[u8], dy_start: usize, out: &mut Vec<u8>) -> bool {
    let mut nibbles = [0u8; LIGHT_BYTES];
    let mut any = false;
    let mut pos = 0;
    for y in 0..16 {
        let layer = (dy_start + y) * LAYER;
        for lz in 0..16 {
            let row = layer + (16 + lz) * GRID + 16;
            for pair in 0..8 {
                let lo = grid[row + pair * 2] & 0x0f;
                let hi = grid[row + pair * 2 + 1] & 0x0f;
                let byte = lo | (hi << 4);
                any |= byte != 0;
                nibbles[pos] = byte;
                pos += 1;
            }
        }
    }
    if !any {
        return false;
    }
    put_varint(LIGHT_BYTES as i32, out);
    out.extend_from_slice(&nibbles);
    true
}

/// Chunks whose light may be affected by editing `pos` (max light spread is 14).
pub(crate) fn affected_light_chunks(pos: BlockPos) -> Vec<[i32; 2]> {
    let [cx, cz] = pos.chunk();
    let lx = pos.0[0].rem_euclid(16);
    let lz = pos.0[2].rem_euclid(16);
    let mut chunks = Vec::with_capacity(9);
    for dz in -1..=1 {
        let dist_z = match dz {
            -1 => lz + 1,
            1 => 16 - lz,
            _ => 0,
        };
        for dx in -1..=1 {
            let dist_x = match dx {
                -1 => lx + 1,
                1 => 16 - lx,
                _ => 0,
            };
            if dist_x + dist_z <= 14 {
                chunks.push([cx + dx, cz + dz]);
            }
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{ChunkGenerator, FlatGenerator};

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

    fn flat_neighborhood(center: Arc<Chunk>, base: &Arc<Chunk>) -> Neighborhood {
        let mut grid: Neighborhood = std::array::from_fn(|_| std::array::from_fn(|_| base.clone()));
        grid[1][1] = center;
        grid
    }

    #[test]
    fn broken_bedrock_and_overhangs_receive_skylight() {
        let generator = FlatGenerator::default();
        let base = generator.generate_chunk(0, 0);
        let mut center = (*base).clone();
        for y in 60..=63 {
            center.set_block(8, y, 8, 0);
        }
        center.set_block(9, 61, 8, 0);

        let payload = chunk_light(&flat_neighborhood(Arc::new(center), &base), None);
        let (sky_mask, sky_arrays, block_mask, _) = decode_light(&payload);
        assert_eq!(block_mask, 0);
        assert_eq!(light_at(sky_mask, &sky_arrays, 8, 60, 8), 15);
        assert_eq!(light_at(sky_mask, &sky_arrays, 8, 59, 8), 15);
        assert_eq!(light_at(sky_mask, &sky_arrays, 9, 61, 8), 14);
    }

    #[test]
    fn block_light_emits_attenuates_and_crosses_chunk_borders() {
        let generator = FlatGenerator::default();
        let base = generator.generate_chunk(0, 0);
        let glowstone = crate::game::block_by_name("glowstone")
            .unwrap()
            .default_state;
        let torch = crate::game::block_by_name("torch").unwrap().default_state;
        let tinted_glass = crate::game::block_by_name("tinted_glass")
            .unwrap()
            .default_state;

        let mut c0 = (*base).clone();
        c0.set_block(15, 64, 8, glowstone);
        c0.set_block(14, 64, 8, tinted_glass);
        c0.set_block(2, 64, 2, torch);
        let c0 = Arc::new(c0);

        let payload0 = chunk_light(&flat_neighborhood(c0.clone(), &base), None);
        let (_, _, b0_mask, b0_arr) = decode_light(&payload0);
        assert_eq!(light_at(b0_mask, &b0_arr, 15, 64, 8), 15);
        assert_eq!(light_at(b0_mask, &b0_arr, 14, 64, 8), 0);
        assert_eq!(light_at(b0_mask, &b0_arr, 15, 65, 8), 14);
        assert_eq!(light_at(b0_mask, &b0_arr, 2, 64, 2), 14);
        assert_eq!(light_at(b0_mask, &b0_arr, 3, 64, 2), 13);

        // Chunk [1, 0] has c0 as its west neighbor ([1][0]).
        let mut grid1: Neighborhood =
            std::array::from_fn(|_| std::array::from_fn(|_| base.clone()));
        grid1[1][0] = c0;
        let payload1 = chunk_light(&grid1, None);
        let (_, _, b1_mask, b1_arr) = decode_light(&payload1);
        assert_eq!(light_at(b1_mask, &b1_arr, 0, 64, 8), 14);
        assert_eq!(light_at(b1_mask, &b1_arr, 1, 64, 8), 13);
        assert_eq!(light_at(b1_mask, &b1_arr, 13, 64, 8), 1);
        assert_eq!(light_at(b1_mask, &b1_arr, 14, 64, 8), 0);
    }

    #[test]
    fn deep_underground_light_sources_below_sky_band_are_lit_and_cross_borders() {
        let generator = FlatGenerator::default();
        let base = generator.generate_chunk(0, 0);
        let torch = crate::game::block_by_name("torch").unwrap().default_state;

        // Place a torch at y = -20 (84 blocks below the intact y = 63 flat surface).
        let mut c0 = (*base).clone();
        c0.set_block(15, -20, 8, torch);
        let c0 = Arc::new(c0);

        let payload0 = chunk_light(&flat_neighborhood(c0.clone(), &base), None);
        let (_, _, b0_mask, b0_arr) = decode_light(&payload0);
        assert_eq!(light_at(b0_mask, &b0_arr, 15, -20, 8), 14);
        assert_eq!(light_at(b0_mask, &b0_arr, 15, -21, 8), 13);
        assert_eq!(light_at(b0_mask, &b0_arr, 15, -19, 8), 13);

        // Neighboring chunk [1, 0] also receives the underground cross-border light.
        let mut grid1: Neighborhood =
            std::array::from_fn(|_| std::array::from_fn(|_| base.clone()));
        grid1[1][0] = c0;
        let payload1 = chunk_light(&grid1, None);
        let (_, _, b1_mask, b1_arr) = decode_light(&payload1);
        assert_eq!(light_at(b1_mask, &b1_arr, 0, -20, 8), 13);
        assert_eq!(light_at(b1_mask, &b1_arr, 12, -20, 8), 1);
        assert_eq!(light_at(b1_mask, &b1_arr, 13, -20, 8), 0);
    }
}
