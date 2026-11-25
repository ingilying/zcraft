//! Vanilla-aligned sky light and block light propagation over a 3x3 chunk
//! neighborhood (Java 1.21.1 / protocol 767).
use super::{
    ALL_LIGHT_SECTIONS, DARK_SKY_SECTIONS, HEIGHT, LIGHT_BYTES, LIGHT_SECTION_COUNT,
    LIT_SKY_SECTIONS, MIN_Y, SURFACE_Y,
    edits::{Edits, Snapshot, base, state_at},
    put_bitset, put_varint,
};
use std::{collections::VecDeque, sync::Arc};

/// 3x3 chunk snapshots ordered as `[dz + 1][dx + 1]` around the center chunk.
pub(crate) type Neighborhood = [[Snapshot; 3]; 3];

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

fn flat_light_payload() -> Arc<[u8]> {
    let mut out =
        Vec::with_capacity(LIT_SKY_SECTIONS.count_ones() as usize * (LIGHT_BYTES + 2) + 32);
    put_bitset(LIT_SKY_SECTIONS, &mut out);
    put_bitset(0, &mut out);
    put_bitset(DARK_SKY_SECTIONS, &mut out);
    put_bitset(ALL_LIGHT_SECTIONS, &mut out);
    put_varint(LIT_SKY_SECTIONS.count_ones() as i32, &mut out);
    for _ in 0..LIT_SKY_SECTIONS.count_ones() {
        put_varint(LIGHT_BYTES as i32, &mut out);
        out.resize(out.len() + LIGHT_BYTES, 0xff);
    }
    put_varint(0, &mut out);
    out.into()
}

pub(crate) fn has_any_edits(neighborhood: &Neighborhood) -> bool {
    neighborhood.iter().flatten().any(Option::is_some)
}

/// Compute protocol 767 sky-light and block-light payload for the center chunk
/// of `neighborhood`, propagating across chunk borders up to 14 blocks.
pub(crate) fn chunk_light(neighborhood: &Neighborhood) -> Arc<[u8]> {
    if !has_any_edits(neighborhood) {
        return flat_light_payload();
    }

    let mut sky_top = [128u16; LAYER];
    let mut min_edit_y = usize::MAX;
    let mut max_edit_y = 0usize;

    for (dz, row) in neighborhood.iter().enumerate() {
        for (dx, snap) in row.iter().enumerate() {
            let Some(edits) = snap.as_deref() else {
                continue;
            };
            if let Some(&first) = edits.keys().next() {
                min_edit_y = min_edit_y.min((first / 256) as usize);
            }
            if let Some(&last) = edits.keys().next_back() {
                max_edit_y = max_edit_y.max((last / 256) as usize);
            }
            let chunk_top = chunk_sky_heights(edits);
            for lz in 0..16 {
                let gz = dz * 16 + lz;
                for lx in 0..16 {
                    let gx = dx * 16 + lx;
                    sky_top[gz * GRID + gx] = chunk_top[lz * 16 + lx];
                }
            }
        }
    }

    let min_top = usize::from(*sky_top.iter().min().unwrap());
    let max_top = usize::from(*sky_top.iter().max().unwrap());
    let min_y = min_top.min(min_edit_y);
    let max_y = max_top.max(max_edit_y.saturating_add(16));
    let band_start = min_y.saturating_sub(15) & !15;
    let band_end = max_y.next_multiple_of(16).min(HEIGHT as usize);
    let band_height = band_end.saturating_sub(band_start);

    let cells = band_height * LAYER;
    let mut atten = vec![0u8; cells];
    let mut sky = vec![0u8; cells];
    let mut block = vec![0u8; cells];
    let mut block_buckets: [Vec<usize>; 16] = std::array::from_fn(|_| Vec::new());

    for dy in 0..band_height {
        let y_rel = band_start + dy;
        let default_state = base(MIN_Y + y_rel as i32);
        let (default_skylight, default_emission, default_atten) = light_props(default_state);
        let _ = default_skylight;
        let layer_base = dy * LAYER;
        for (dz, row) in neighborhood.iter().enumerate() {
            for (dx, snap) in row.iter().enumerate() {
                let edits = snap.as_deref();
                for lz in 0..16 {
                    let gz = dz * 16 + lz;
                    let row_offset = layer_base + gz * GRID + dx * 16;
                    let top_offset = gz * GRID + dx * 16;
                    for lx in 0..16 {
                        let idx = row_offset + lx;
                        let (emission, opacity) = match edits {
                            Some(edits) => {
                                let state = state_at(edits, (y_rel * 256 + lz * 16 + lx) as u32);
                                let (_, e, a) = light_props(state);
                                (e, a)
                            }
                            None => (default_emission, default_atten),
                        };
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
    // Process strictly from highest light level (15) down to 2 so every cell
    // settles on its first visit without redundant re-queuing.
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

fn chunk_sky_heights(edits: &Edits) -> [u16; 256] {
    let mut result = [0u16; 256];
    for (col, height) in result.iter_mut().enumerate() {
        for y in (60..=SURFACE_Y).rev() {
            if !allows_direct_skylight(state_at(edits, ((y - MIN_Y) as u32) * 256 + col as u32)) {
                *height = (y - MIN_Y + 1) as u16;
                break;
            }
        }
    }
    for (&i, &state) in edits {
        if !allows_direct_skylight(state) {
            let height = (i / 256 + 1) as u16;
            result[i as usize % 256] = result[i as usize % 256].max(height);
        }
    }
    result
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
pub(crate) fn affected_light_chunks(pos: super::BlockPos) -> Vec<[i32; 2]> {
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
