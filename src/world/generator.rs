//! Extensible `ChunkGenerator` trait and configurable Superflat implementation.
use super::{
    ALL_LIGHT_SECTIONS, Chunk, ChunkSection, HEIGHT, LIGHT_BYTES, LIGHT_SECTION_COUNT, MIN_Y,
    SECTION_COUNT, put_bitset, put_varint,
};
use serde::{Deserialize, Serialize};
use std::{fmt, io, sync::Arc};

/// Pluggable terrain generator producing section-based `Chunk`s.
pub trait ChunkGenerator: Send + Sync + fmt::Debug {
    /// Generate a chunk at chunk coordinates `(cx, cz)`.
    fn generate_chunk(&self, cx: i32, cz: i32) -> Arc<Chunk>;

    /// Default world spawn position `[x, y, z]`.
    fn spawn_position(&self) -> [f64; 3];

    /// Optional shared immutable chunk template for uniform worlds (e.g. Superflat).
    fn default_chunk(&self) -> Option<&Arc<Chunk>> {
        None
    }

    /// Optional pre-encoded Chunk Data + Light payload matching `default_chunk()`.
    fn default_terrain_payload(&self) -> Option<&Arc<[u8]>> {
        None
    }

    /// Optional pre-encoded Update Light payload matching `default_chunk()`.
    fn default_light_payload(&self) -> Option<&Arc<[u8]>> {
        None
    }

    /// Optional Superflat config for `level.dat` serialization.
    fn flat_config(&self) -> Option<&FlatGeneratorConfig> {
        None
    }
}

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

/// Compiled Superflat generator with a shared immutable chunk template and
/// pre-encoded terrain/light payloads.
#[derive(Debug, Clone)]
pub struct FlatGenerator {
    config: FlatGeneratorConfig,
    biome_id: i32,
    surface_y: i32,
    sky_top: u16,
    spawn_position: [f64; 3],
    template: Arc<Chunk>,
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
        let biome_id = crate::registry_data::biome_id(&config.biome)
            .expect("validated flat biome must exist in registry");

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

        let sections: Box<[ChunkSection; SECTION_COUNT]> = Box::new(std::array::from_fn(|sec| {
            let start = sec * 16;
            let slice = &column[start..start + 16];
            let first = slice[0];
            if slice.iter().all(|&s| s == first) {
                return ChunkSection::filled(first, biome_id);
            }
            let mut section = ChunkSection::empty(biome_id);
            for (ly, &state) in slice.iter().enumerate() {
                if state != crate::game::AIR_STATE {
                    section.fill_layer(ly, state);
                }
            }
            section
        }));
        let template = Arc::new(Chunk::from_sections(sections));

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

        let light_payload: Arc<[u8]> = encode_flat_light(&column, sky_top).into();
        let terrain_payload = template.encode_payload(&light_payload);

        Ok(Self {
            config,
            biome_id,
            surface_y,
            sky_top,
            spawn_position,
            template,
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
        self.template.get_block(0, y, 0)
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

    pub fn terrain_payload(&self) -> &Arc<[u8]> {
        &self.terrain_payload
    }
}

impl ChunkGenerator for FlatGenerator {
    fn generate_chunk(&self, _cx: i32, _cz: i32) -> Arc<Chunk> {
        self.template.clone()
    }

    fn spawn_position(&self) -> [f64; 3] {
        self.spawn_position
    }

    fn default_chunk(&self) -> Option<&Arc<Chunk>> {
        Some(&self.template)
    }

    fn default_terrain_payload(&self) -> Option<&Arc<[u8]>> {
        Some(&self.terrain_payload)
    }

    fn default_light_payload(&self) -> Option<&Arc<[u8]>> {
        Some(&self.light_payload)
    }

    fn flat_config(&self) -> Option<&FlatGeneratorConfig> {
        Some(&self.config)
    }
}

#[inline]
fn allows_direct_skylight(state: i32) -> bool {
    crate::game::block_by_state(state).is_some_and(|def| def.skylight && def.light_block == 0)
}

fn encode_flat_light(column: &[i32; HEIGHT as usize], sky_top: u16) -> Vec<u8> {
    let mut sky_levels = [0u8; HEIGHT as usize];
    for (y_rel, level) in sky_levels.iter_mut().enumerate() {
        if y_rel >= usize::from(sky_top) {
            *level = 15;
        }
    }
    let mut block_levels = [0u8; HEIGHT as usize];
    for (y_rel, &state) in column.iter().enumerate() {
        if let Some(def) = crate::game::block_by_state(state) {
            block_levels[y_rel] = def.light_emission.min(15);
        }
    }
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
