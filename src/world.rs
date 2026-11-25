//! Static, deterministic flat chunks for Minecraft Java 1.21.1 (protocol 767).
//!
//! Registry contract: overworld dimension type at dynamic ID 0, min_y -64,
//! height 384, and plains biome at dynamic ID 0. The terrain is bedrock at y=60,
//! dirt at y=61..=62, grass at y=63, and air elsewhere. Creative edits are
//! in memory, with vertical skylight only and no block entities.
//!
//! Versioned wire/ID references (numeric facts only; no bundled game assets):
//! - https://github.com/PrismarineJS/minecraft-data/blob/master/data/pc/1.21.1/protocol.json
//!   `packet_map_chunk`, clientbound Play ID 0x27; heightmaps are anonymous NBT.
//! - https://github.com/PrismarineJS/minecraft-data/blob/master/data/pc/1.21.1/blocks.json
//!   defaultState IDs: air=0, bedrock=79, dirt=10, grass_block=9 (snowy=false).
//! - https://minecraft.wiki/w/Java_Edition_protocol/Chunk_format
//!   Sections bottom-up; x fastest, then z, then y. Packed values do not cross
//!   64-bit boundaries. Protocol 767 still includes the VarInt long-array length
//!   (removed in 1.21.5). Heightmap values are first air Y relative to min_y.
//! - https://minecraft.wiki/w/Java_Edition_protocol/Packets#Chunk_Data_and_Update_Light
//!   Light bits cover the dimension's sections plus one boundary at each end;
//!   arrays follow set bits in ascending order, with 2048 bytes per nibble array.

mod edits;
mod light;
pub use edits::BlockPos;
pub(crate) use edits::{BlockWorld, Snapshot, changed_blocks, chunk_payload_with_light};
pub(crate) use light::{Neighborhood, affected_light_chunks, chunk_light, has_any_edits};

use std::io;

pub const MIN_Y: i32 = -64;
pub const HEIGHT: i32 = 384;
pub const SPAWN_X: f64 = 8.5;
pub const SPAWN_Y: f64 = 65.0;
pub const SPAWN_Z: f64 = 8.5;
pub const VIEW_DISTANCE: i32 = 2;
pub const MAX_VIEW_DISTANCE: i32 = 16;
pub const WORLD_LIMIT: f64 = 29_999_984.0;

pub(crate) fn chunk_at(x: f64, z: f64) -> [i32; 2] {
    [(x / 16.0).floor() as i32, (z / 16.0).floor() as i32]
}

pub(crate) fn validate_view_distance(distance: i32) -> io::Result<()> {
    if (VIEW_DISTANCE..=MAX_VIEW_DISTANCE).contains(&distance) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "max_view_distance must be 2..=16",
        ))
    }
}

const SECTION_COUNT: usize = (HEIGHT / 16) as usize;
const LIGHT_SECTION_COUNT: usize = SECTION_COUNT + 2;
const SURFACE_Y: i32 = 63;
const GROUND_SECTION: usize = ((SURFACE_Y - MIN_Y) / 16) as usize;
const BLOCK_PALETTE: [i32; 4] = [
    crate::game::AIR_STATE,
    crate::game::BEDROCK_STATE,
    crate::game::DIRT_STATE,
    crate::game::GRASS_BLOCK_STATE,
];
const PLAINS_BIOME_ID: i32 = 0;
const LIGHT_BYTES: usize = 2048;
const ALL_LIGHT_SECTIONS: u64 = (1 << LIGHT_SECTION_COUNT) - 1;
// Light index 0 is the extra section below min_y. Ground is section 7,
// represented by light bit 8, and the first section above ground uses bit 9.
const DARK_SKY_SECTIONS: u64 = (1 << (GROUND_SECTION + 2)) - 1;
const LIT_SKY_SECTIONS: u64 = ALL_LIGHT_SECTIONS ^ DARK_SKY_SECTIONS;

/// Encode the body of clientbound Play Chunk Data and Update Light (0x27).
/// The caller supplies the packet ID and frame length. Arbitrary coordinates
/// intentionally produce identical terrain, so neighboring chunks join cleanly.
pub fn chunk_data(x: i32, z: i32) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(40 * 1024);
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&z.to_be_bytes());
    put_chunk_payload(&mut out);
    Ok(out)
}

// All coordinates share exactly one immutable terrain/light payload.
pub(crate) fn chunk_payload() -> std::sync::Arc<[u8]> {
    let mut out = Vec::with_capacity(40 * 1024);
    put_chunk_payload(&mut out);
    out.into()
}

fn put_chunk_payload(out: &mut Vec<u8>) {
    put_heightmaps(out);

    let sections = section_data();
    put_varint(sections.len() as i32, out);
    out.extend_from_slice(&sections);
    put_varint(0, out); // Block entity count.

    put_bitset(LIT_SKY_SECTIONS, out);
    put_bitset(0, out); // No nonzero emitted block light.
    put_bitset(DARK_SKY_SECTIONS, out);
    put_bitset(ALL_LIGHT_SECTIONS, out); // Explicitly clear all block light.

    put_varint(LIT_SKY_SECTIONS.count_ones() as i32, out);
    for _ in 0..LIT_SKY_SECTIONS.count_ones() {
        put_varint(LIGHT_BYTES as i32, out);
        // Two level-15 sky-light samples per byte. The slab ends at a section
        // boundary; no partially lit section is required. Air under the solid,
        // infinite slab receives no sky light. Air at/above y=64 is full daylight.
        out.resize(out.len() + LIGHT_BYTES, 0xff);
    }
    put_varint(0, out); // Block-light array count.
}

fn put_heightmaps(out: &mut Vec<u8>) {
    put_heightmaps_with(out, &[(SURFACE_Y + 1 - MIN_Y) as u16; 256]);
}

fn put_heightmaps_with(out: &mut Vec<u8>, heights: &[u16; 256]) {
    out.push(10); // Anonymous root TAG_Compound, without a root-name field.
    for name in ["MOTION_BLOCKING", "WORLD_SURFACE"] {
        out.push(12); // Named TAG_Long_Array.
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        const ENTRIES_PER_LONG: usize = 64 / 9;
        const LONG_COUNT: usize = 256_usize.div_ceil(ENTRIES_PER_LONG);
        out.extend_from_slice(&(LONG_COUNT as i32).to_be_bytes());
        for heights in heights.chunks(ENTRIES_PER_LONG) {
            let mut packed = 0u64;
            for (index, &height) in heights.iter().enumerate() {
                packed |= u64::from(height) << (index * 9);
            }
            out.extend_from_slice(&packed.to_be_bytes());
        }
    }
    out.push(0); // TAG_End.
}

fn section_data() -> Vec<u8> {
    let mut out = Vec::with_capacity(2300);
    for section in 0..SECTION_COUNT {
        if section == GROUND_SECTION {
            out.extend_from_slice(&1024u16.to_be_bytes()); // Four solid 16x16 layers.
            out.push(4); // Indirect block palette minimum: four bits per entry.
            put_varint(BLOCK_PALETTE.len() as i32, &mut out);
            for state in BLOCK_PALETTE {
                put_varint(state, &mut out);
            }
            put_varint(256, &mut out); // 4096 four-bit entries / 16 entries per long.
            for local_y in 0..16 {
                let palette_index = match local_y {
                    12 => 1,      // y=60: bedrock.
                    13 | 14 => 2, // y=61,62: dirt.
                    15 => 3,      // y=63: grass.
                    _ => 0,
                };
                let packed = 0x1111_1111_1111_1111u64 * palette_index;
                // Each long spans one row of 16 x-coordinates. Sixteen rows
                // cover the z dimension of this horizontal layer.
                for _ in 0..16 {
                    out.extend_from_slice(&packed.to_be_bytes());
                }
            }
        } else {
            out.extend_from_slice(&0u16.to_be_bytes()); // Non-air block count.
            put_single_palette(0, &mut out); // Entire section is air.
        }
        // Every section contains 64 plains biome samples. Single-valued
        // containers omit palette length and data, but keep data-length zero.
        put_single_palette(PLAINS_BIOME_ID, &mut out);
    }
    out
}

fn put_single_palette(value: i32, out: &mut Vec<u8>) {
    out.push(0); // Bits per entry.
    put_varint(value, out); // Sole global palette value, without palette length.
    put_varint(0, out); // Empty long-array length, required in protocol 767.
}

fn put_bitset(value: u64, out: &mut Vec<u8>) {
    if value == 0 {
        put_varint(0, out);
    } else {
        put_varint(1, out);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn put_varint(value: i32, out: &mut Vec<u8>) {
    let mut value = value as u32;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(byte | if value == 0 { 0 } else { 0x80 });
        if value == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent cursor used to verify all wire lengths and finish exactly at
    /// the packet boundary, without depending on the packet serializer helpers.
    struct Cursor<'a>(&'a [u8]);

    impl<'a> Cursor<'a> {
        fn take(&mut self, n: usize) -> &'a [u8] {
            let (head, tail) = self.0.split_at(n);
            self.0 = tail;
            head
        }
        fn byte(&mut self) -> u8 {
            self.take(1)[0]
        }
        fn short(&mut self) -> u16 {
            u16::from_be_bytes(self.take(2).try_into().unwrap())
        }
        fn int(&mut self) -> i32 {
            i32::from_be_bytes(self.take(4).try_into().unwrap())
        }
        fn long(&mut self) -> u64 {
            u64::from_be_bytes(self.take(8).try_into().unwrap())
        }
        fn varint(&mut self) -> usize {
            let mut value = 0usize;
            for i in 0..5 {
                let byte = self.byte();
                value |= usize::from(byte & 0x7f) << (7 * i);
                if byte & 0x80 == 0 {
                    return value;
                }
            }
            panic!("invalid VarInt");
        }
        fn bitset(&mut self) -> u64 {
            match self.varint() {
                0 => 0,
                1 => self.long(),
                length => panic!("unexpected BitSet length {length}"),
            }
        }
        fn heightmaps(&mut self) {
            assert_eq!(self.byte(), 10);
            for expected in ["MOTION_BLOCKING", "WORLD_SURFACE"] {
                assert_eq!(self.byte(), 12);
                let name_len = self.short() as usize;
                assert_eq!(self.take(name_len), expected.as_bytes());
                assert_eq!(self.int(), 37);
                for i in 0..37 {
                    let packed = self.long();
                    let count = (256 - i * 7).min(7);
                    for j in 0..count {
                        assert_eq!((packed >> (j * 9)) & 0x1ff, 128);
                    }
                    assert_eq!(packed >> (count * 9), 0, "heightmap padding");
                }
            }
            assert_eq!(self.byte(), 0);
        }
    }

    #[test]
    fn dimension_spawn_and_versioned_block_ids_match_contract() {
        assert_eq!((MIN_Y, HEIGHT, SECTION_COUNT), (-64, 384, 24));
        assert_eq!((SPAWN_X, SPAWN_Y, SPAWN_Z), (8.5, 65.0, 8.5));
        assert_eq!(VIEW_DISTANCE, 2);
        assert_eq!(BLOCK_PALETTE, [0, 79, 10, 9]);
        assert_eq!(PLAINS_BIOME_ID, 0);
        assert_eq!(GROUND_SECTION, 7);
    }

    #[test]
    fn heightmaps_are_anonymous_nbt_and_pack_first_air_relative_to_min_y() {
        let body = chunk_data(0, 0).unwrap();
        let mut cursor = Cursor(&body[8..]);
        cursor.heightmaps();
        assert!(!cursor.0.is_empty(), "section data follows TAG_End");
    }

    #[test]
    fn all_24_sections_decode_to_the_expected_floor_and_plains_biomes() {
        let bytes = section_data();
        let mut cursor = Cursor(&bytes);
        let mut total_solids = 0;
        for section in 0..24 {
            let non_air = cursor.short() as usize;
            let mut decoded = vec![0; 4096];
            match cursor.byte() {
                0 => {
                    let state = cursor.varint();
                    assert_eq!(cursor.varint(), 0);
                    decoded.fill(state);
                }
                4 => {
                    assert_eq!(cursor.varint(), 4);
                    let palette: Vec<_> = (0..4).map(|_| cursor.varint()).collect();
                    assert_eq!(palette, [0, 79, 10, 9]);
                    assert_eq!(cursor.varint(), 256);
                    for long_index in 0..256 {
                        let value = cursor.long();
                        for j in 0..16 {
                            decoded[long_index * 16 + j] =
                                palette[((value >> (j * 4)) & 15) as usize];
                        }
                    }
                }
                other => panic!("unexpected palette bit width {other}"),
            }
            for (index, &actual) in decoded.iter().enumerate() {
                // Wire order: x + 16*z + 256*y.
                let y = -64 + section * 16 + index as i32 / 256;
                let expected = match y {
                    60 => 79,
                    61 | 62 => 10,
                    63 => 9,
                    _ => 0,
                };
                assert_eq!(actual, expected, "section {section}, block {index}");
            }
            assert_eq!(decoded.iter().filter(|&&state| state != 0).count(), non_air);
            total_solids += non_air;
            assert_eq!(cursor.byte(), 0); // Biome bits per entry.
            assert_eq!(cursor.varint(), 0); // Dynamic plains ID.
            assert_eq!(cursor.varint(), 0); // Biome long-array length.
        }
        assert_eq!(total_solids, 1024);
        assert!(cursor.0.is_empty());
    }

    #[test]
    fn chunk_light_masks_cover_all_26_sections_and_arrays_follow_set_bits() {
        let body = chunk_data(-12, 31).unwrap();
        let mut cursor = Cursor(&body);
        assert_eq!((cursor.int(), cursor.int()), (-12, 31));
        cursor.heightmaps();
        let section_len = cursor.varint();
        assert_eq!(cursor.take(section_len), section_data());
        assert_eq!(cursor.varint(), 0); // Block entities.
        let sky = cursor.bitset();
        let block = cursor.bitset();
        let empty_sky = cursor.bitset();
        let empty_block = cursor.bitset();
        assert_eq!(sky | empty_sky, (1 << 26) - 1);
        assert_eq!(sky & empty_sky, 0);
        assert_eq!(block | empty_block, (1 << 26) - 1);
        assert_eq!(block & empty_block, 0);
        assert_eq!(empty_sky, 0x1ff);
        assert_eq!(block, 0);
        assert_eq!(empty_block, (1 << 26) - 1);
        assert_eq!(cursor.varint(), 17);
        let mut arrays = 0;
        for bit in 0..26 {
            let section_base_y = -80 + bit * 16;
            if sky & (1 << bit) != 0 {
                assert!(section_base_y >= 64);
                assert_eq!(cursor.varint(), 2048);
                let samples = cursor.take(2048);
                assert!(samples.iter().all(|&byte| byte == 0xff));
                arrays += 1;
            } else {
                assert!(section_base_y < 64);
            }
        }
        assert_eq!(arrays, sky.count_ones());
        assert_eq!(cursor.varint(), 0); // Block-light arrays.
        assert!(cursor.0.is_empty(), "no trailing or missing bytes");
    }

    #[test]
    fn coordinates_are_big_endian_and_remaining_terrain_is_identical() {
        let zero = chunk_data(0, 0).unwrap();
        for (x, z) in [(-1, 1), (i32::MIN, i32::MAX), (300, -400)] {
            let body = chunk_data(x, z).unwrap();
            assert_eq!(&body[..4], &x.to_be_bytes());
            assert_eq!(&body[4..8], &z.to_be_bytes());
            assert_eq!(&body[8..], &zero[8..]);
        }
    }

    #[test]
    fn deterministic_payload_fits_uncompressed_frame_limit() {
        let body = chunk_data(0, 0).unwrap();
        assert_eq!(body, chunk_data(0, 0).unwrap());
        assert!(body.len() + 1 < 2_097_151); // Include one-byte packet ID.
        assert!(body.len() < 40 * 1024);
        assert!(body.len() > 17 * 2048); // Sky-light arrays are actually present.
    }
}
