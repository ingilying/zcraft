//! Configurable Superflat terrain generator (extensible for future generators).
use super::{
    ALL_LIGHT_SECTIONS, HEIGHT, LIGHT_BYTES, LIGHT_SECTION_COUNT, MIN_Y, SECTION_COUNT,
    pack_entries, put_bitset, put_heightmaps_with, put_single_palette, put_varint,
};
use serde::{Deserialize, Serialize};
use std::{io, sync::Arc};

/// One horizontal layer in a Superflat preset, stacked bottom-up from `base_y`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlatLayerConfig {
    pub block: String,
    pub height: u16,
}

/// Configuration for the Superflat world generator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlatGeneratorConfig {
    /// World Y coordinate where the first layer starts (default: 60).
    pub base_y: i32,
    /// Biome resource location (default: `minecraft:plains`).
    pub biome: String,
    /// Horizontal layers stacked bottom-up from `base_y`.
    pub layers: Vec<FlatLayerConfig>,
}

impl Default for FlatGeneratorConfig {
    fn default() -> Self {
        Self {
            base_y: 60,
            biome: "minecraft:plains".into(),
            layers: vec![
                FlatLayerConfig {
                    block: "minecraft:bedrock".into(),
                    height: 1,
                },
                FlatLayerConfig {
                    block: "minecraft:dirt".into(),
                    height: 2,
                },
                FlatLayerConfig {
                    block: "minecraft:grass_block".into(),
                    height: 1,
                },
            ],
        }
    }
}

impl FlatGeneratorConfig {
    pub fn validate(&self) -> io::Result<()> {
        let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidInput, msg);
        if !(MIN_Y..MIN_Y + HEIGHT).contains(&self.base_y) {
            return Err(invalid(
                "flat.base_y must be within world height (-64..320)",
            ));
        }
        if self.biome.trim().is_empty() {
            return Err(invalid("flat.biome must not be empty"));
        }
        if crate::registry_data::biome_id(&self.biome).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown flat biome '{}'", self.biome),
            ));
        }
        let mut cursor = self.base_y;
        for layer in &self.layers {
            if layer.height == 0 {
                return Err(invalid("flat layer height must be at least 1"));
            }
            if crate::game::block_by_name(&layer.block).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown flat layer block '{}'", layer.block),
                ));
            }
            cursor = cursor
                .checked_add(i32::from(layer.height))
                .ok_or_else(|| invalid("flat layers overflow world height"))?;
            if cursor > MIN_Y + HEIGHT {
                return Err(invalid("flat layers exceed maximum world height (320)"));
            }
        }
        Ok(())
    }
}

/// Compiled Superflat generator with precomputed column states, heightmaps,
/// sky-light payload, and immutable chunk template.
#[derive(Debug, Clone)]
pub struct FlatGenerator {
    config: FlatGeneratorConfig,
    biome_id: i32,
    column: Box<[i32; HEIGHT as usize]>,
    section_has_blocks: [bool; SECTION_COUNT],
    surface_y: i32,
    sky_top: u16,
    spawn_position: [f64; 3],
    heightmap: [u16; 256],
    section_data: Arc<[u8]>,
    light_payload: Arc<[u8]>,
    terrain_payload: Arc<[u8]>,
}

impl Default for FlatGenerator {
    fn default() -> Self {
        Self::from_config(FlatGeneratorConfig::default())
            .expect("default flat generator config must be valid")
    }
}

impl FlatGenerator {
    pub fn from_config(config: FlatGeneratorConfig) -> io::Result<Self> {
        config.validate()?;
        let mut column = Box::new([crate::game::AIR_STATE; HEIGHT as usize]);
        let mut y = config.base_y;
        for layer in &config.layers {
            let state = crate::game::block_by_name(&layer.block)
                .expect("validated block name")
                .default_state;
            for _ in 0..layer.height {
                column[(y - MIN_Y) as usize] = state;
                y += 1;
            }
        }

        let mut section_has_blocks = [false; SECTION_COUNT];
        for (sec, has_blocks) in section_has_blocks.iter_mut().enumerate() {
            let start = sec * 16;
            *has_blocks = column[start..start + 16]
                .iter()
                .any(|&s| s != crate::game::AIR_STATE);
        }

        let surface_y = column
            .iter()
            .rposition(|&s| s != crate::game::AIR_STATE)
            .map_or(MIN_Y - 1, |idx| MIN_Y + idx as i32);

        let sky_top = column
            .iter()
            .rposition(|&s| !allows_direct_skylight(s))
            .map_or(0u16, |idx| (idx + 1) as u16);

        let spawn_y = if surface_y >= MIN_Y {
            f64::from(surface_y + 2)
        } else {
            super::SPAWN_Y
        };
        let spawn_position = [super::SPAWN_X, spawn_y, super::SPAWN_Z];

        let height_value = if surface_y >= MIN_Y {
            (surface_y + 1 - MIN_Y) as u16
        } else {
            0
        };
        let heightmap = [height_value; 256];

        let biome_id = crate::registry_data::biome_id(&config.biome)
            .expect("validated flat biome must exist in registry");
        let section_data: Arc<[u8]> =
            encode_flat_sections(&column, &section_has_blocks, biome_id).into();
        let light_payload: Arc<[u8]> = encode_flat_light(&column, sky_top).into();
        let terrain_payload: Arc<[u8]> =
            encode_flat_chunk_payload(&heightmap, &section_data, &light_payload).into();

        Ok(Self {
            config,
            biome_id,
            column,
            section_has_blocks,
            surface_y,
            sky_top,
            spawn_position,
            heightmap,
            section_data,
            light_payload,
            terrain_payload,
        })
    }

    pub fn config(&self) -> &FlatGeneratorConfig {
        &self.config
    }

    #[inline]
    pub fn biome_id(&self) -> i32 {
        self.biome_id
    }

    #[inline]
    pub fn base_state(&self, y: i32) -> i32 {
        let idx = y - MIN_Y;
        if (0..HEIGHT).contains(&idx) {
            self.column[idx as usize]
        } else {
            crate::game::AIR_STATE
        }
    }

    #[inline]
    pub fn has_blocks_in_section(&self, section: usize) -> bool {
        self.section_has_blocks
            .get(section)
            .copied()
            .unwrap_or(false)
    }

    #[inline]
    pub fn surface_y(&self) -> i32 {
        self.surface_y
    }

    #[inline]
    pub fn sky_top(&self) -> u16 {
        self.sky_top
    }

    #[inline]
    pub fn spawn_position(&self) -> [f64; 3] {
        self.spawn_position
    }

    pub fn heightmap(&self) -> &[u16; 256] {
        &self.heightmap
    }

    pub fn section_data(&self) -> &[u8] {
        &self.section_data
    }

    pub fn light_payload(&self) -> &Arc<[u8]> {
        &self.light_payload
    }

    pub fn terrain_payload(&self) -> &Arc<[u8]> {
        &self.terrain_payload
    }

    pub fn column(&self) -> &[i32; HEIGHT as usize] {
        &self.column
    }
}

#[inline]
fn allows_direct_skylight(state: i32) -> bool {
    crate::game::block_by_state(state).is_some_and(|def| def.skylight && def.light_block == 0)
}

fn encode_flat_sections(
    column: &[i32; HEIGHT as usize],
    section_has_blocks: &[bool; SECTION_COUNT],
    biome_id: i32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(2300);
    for (section, &has_blocks) in section_has_blocks.iter().enumerate() {
        if !has_blocks {
            out.extend_from_slice(&0u16.to_be_bytes());
            put_single_palette(crate::game::AIR_STATE, &mut out);
            put_single_palette(biome_id, &mut out);
            continue;
        }
        let base_idx = section * 16;
        let slice = &column[base_idx..base_idx + 16];
        let non_air = slice
            .iter()
            .filter(|&&s| s != crate::game::AIR_STATE)
            .count() as u16
            * 256;
        out.extend_from_slice(&non_air.to_be_bytes());

        let mut palette = Vec::new();
        let mut layer_indices = [0u64; 16];
        for (ly, &state) in slice.iter().enumerate() {
            layer_indices[ly] = palette.iter().position(|&s| s == state).unwrap_or_else(|| {
                palette.push(state);
                palette.len() - 1
            }) as u64;
        }

        if palette.len() == 1 {
            put_single_palette(palette[0], &mut out);
        } else {
            let bits = ((palette.len() - 1).ilog2() as u8 + 1).max(4);
            out.push(bits);
            put_varint(palette.len() as i32, &mut out);
            for state in palette {
                put_varint(state, &mut out);
            }
            let mut entries = [0u64; 4096];
            for (ly, &idx) in layer_indices.iter().enumerate() {
                entries[ly * 256..(ly + 1) * 256].fill(idx);
            }
            pack_entries(&entries, bits, &mut out);
        }
        put_single_palette(biome_id, &mut out);
    }
    out
}

fn encode_flat_light(column: &[i32; HEIGHT as usize], sky_top: u16) -> Vec<u8> {
    let mut sky_levels = [0u8; HEIGHT as usize];
    for (y_rel, level) in sky_levels.iter_mut().enumerate() {
        if y_rel >= usize::from(sky_top) {
            *level = 15;
        }
    }
    // Check if any flat layer emits block light.
    let mut block_levels = [0u8; HEIGHT as usize];
    for (y_rel, &state) in column.iter().enumerate() {
        if let Some(def) = crate::game::block_by_state(state) {
            block_levels[y_rel] = def.light_emission.min(15);
        }
    }
    // Propagate vertical block light in uniform flat layers if any emitter exists.
    if block_levels.iter().any(|&v| v > 0) {
        for y in 1..HEIGHT as usize {
            let atten = crate::game::block_by_state(column[y])
                .map_or(15, |d| d.light_block.min(15))
                .max(1);
            let propagated = block_levels[y - 1].saturating_sub(atten);
            block_levels[y] = block_levels[y].max(propagated);
        }
        for y in (0..HEIGHT as usize - 1).rev() {
            let atten = crate::game::block_by_state(column[y])
                .map_or(15, |d| d.light_block.min(15))
                .max(1);
            let propagated = block_levels[y + 1].saturating_sub(atten);
            block_levels[y] = block_levels[y].max(propagated);
        }
    }

    let mut sky_mask = 0u64;
    let mut block_mask = 0u64;
    let mut sky_arrays = Vec::new();
    let mut block_arrays = Vec::new();

    for section in 1..LIGHT_SECTION_COUNT {
        let bottom = (section - 1) * 16;
        if bottom >= HEIGHT as usize {
            // Top boundary section above max_y is always full skylight (15).
            sky_mask |= 1 << section;
            put_varint(LIGHT_BYTES as i32, &mut sky_arrays);
            sky_arrays.resize(sky_arrays.len() + LIGHT_BYTES, 0xff);
            continue;
        }
        let sky_slice = &sky_levels[bottom..bottom + 16];
        if sky_slice.iter().any(|&v| v != 0) {
            sky_mask |= 1 << section;
            put_varint(LIGHT_BYTES as i32, &mut sky_arrays);
            for &level in sky_slice {
                let byte = (level & 0x0f) | ((level & 0x0f) << 4);
                sky_arrays.resize(sky_arrays.len() + 128, byte);
            }
        }
        let block_slice = &block_levels[bottom..bottom + 16];
        if block_slice.iter().any(|&v| v != 0) {
            block_mask |= 1 << section;
            put_varint(LIGHT_BYTES as i32, &mut block_arrays);
            for &level in block_slice {
                let byte = (level & 0x0f) | ((level & 0x0f) << 4);
                block_arrays.resize(block_arrays.len() + 128, byte);
            }
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
    out
}

fn encode_flat_chunk_payload(
    heightmap: &[u16; 256],
    section_data: &[u8],
    light_payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(section_data.len() + light_payload.len() + 1024);
    put_heightmaps_with(&mut out, heightmap);
    put_varint(section_data.len() as i32, &mut out);
    out.extend_from_slice(section_data);
    put_varint(0, &mut out); // Block entity count.
    out.extend_from_slice(light_payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_flat_generator_matches_legacy_chunk_payload_byte_for_byte() {
        let generator = FlatGenerator::default();
        assert_eq!(generator.surface_y(), 63);
        assert_eq!(generator.sky_top(), 128);
        assert_eq!(generator.spawn_position(), [8.5, 65.0, 8.5]);
        assert_eq!(
            generator.terrain_payload().as_ref(),
            super::super::chunk_payload().as_ref()
        );
    }

    #[test]
    fn custom_flat_preset_at_min_y_computes_surface_spawn_and_valid_payload() {
        let cfg = FlatGeneratorConfig {
            base_y: -64,
            biome: "minecraft:plains".into(),
            layers: vec![
                FlatLayerConfig {
                    block: "minecraft:bedrock".into(),
                    height: 1,
                },
                FlatLayerConfig {
                    block: "minecraft:stone".into(),
                    height: 3,
                },
                FlatLayerConfig {
                    block: "minecraft:grass_block".into(),
                    height: 1,
                },
            ],
        };
        let generator = FlatGenerator::from_config(cfg).unwrap();
        assert_eq!(generator.base_state(-64), crate::game::BEDROCK_STATE);
        assert_eq!(generator.base_state(-63), crate::game::STONE_STATE);
        assert_eq!(generator.base_state(-60), crate::game::GRASS_BLOCK_STATE);
        assert_eq!(generator.base_state(-59), crate::game::AIR_STATE);
        assert_eq!(generator.surface_y(), -60);
        assert_eq!(generator.sky_top(), 5);
        assert_eq!(generator.spawn_position(), [8.5, -58.0, 8.5]);
        assert!(!generator.terrain_payload().is_empty());
    }
}
