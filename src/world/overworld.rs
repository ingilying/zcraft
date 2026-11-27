//! Deterministic Overworld natural terrain generator (Java 1.21.1).
//!
//! Produces multi-biome terrain (oceans, beaches, plains, forests, deserts,
//! taiga, mountains) with 2D FBM heightmaps, 3D cave carving, deepslate below
//! y=0, vanilla-range ore pockets, and chunk-safe trees/foliage.
use super::{Chunk, ChunkGenerator, ChunkSection, HEIGHT, MIN_Y, SECTION_COUNT};
use crate::{game::block_by_name, registry_data::biome_id};
use std::sync::Arc;

pub const SEA_LEVEL: i32 = 63;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BiomeKind {
    DeepOcean,
    Ocean,
    Beach,
    Desert,
    Plains,
    Forest,
    Taiga,
    Mountains,
    SnowyPeaks,
}

#[derive(Debug, Clone, Copy)]
struct Palette {
    air: i32,
    bedrock: i32,
    stone: i32,
    deepslate: i32,
    dirt: i32,
    grass_block: i32,
    sand: i32,
    sandstone: i32,
    gravel: i32,
    water: i32,
    lava: i32,
    snow_block: i32,
    oak_log: i32,
    oak_leaves: i32,
    birch_log: i32,
    birch_leaves: i32,
    spruce_log: i32,
    spruce_leaves: i32,
    short_grass: i32,
    poppy: i32,
    dandelion: i32,
    cactus: i32,
    coal_ore: i32,
    iron_ore: i32,
    copper_ore: i32,
    gold_ore: i32,
    redstone_ore: i32,
    diamond_ore: i32,
    deepslate_iron_ore: i32,
    deepslate_gold_ore: i32,
    deepslate_redstone_ore: i32,
    deepslate_diamond_ore: i32,
    biome_deep_ocean: i32,
    biome_ocean: i32,
    biome_beach: i32,
    biome_desert: i32,
    biome_plains: i32,
    biome_forest: i32,
    biome_taiga: i32,
    biome_mountains: i32,
    biome_snowy_peaks: i32,
}

impl Palette {
    fn load() -> Self {
        let b = |name: &str| block_by_name(name).expect(name).default_state;
        let bio = |name: &str| biome_id(name).unwrap_or(0);
        Self {
            air: crate::game::AIR_STATE,
            bedrock: b("bedrock"),
            stone: b("stone"),
            deepslate: b("deepslate"),
            dirt: b("dirt"),
            grass_block: b("grass_block"),
            sand: b("sand"),
            sandstone: b("sandstone"),
            gravel: b("gravel"),
            water: b("water"),
            lava: b("lava"),
            snow_block: b("snow_block"),
            oak_log: b("oak_log"),
            oak_leaves: b("oak_leaves"),
            birch_log: b("birch_log"),
            birch_leaves: b("birch_leaves"),
            spruce_log: b("spruce_log"),
            spruce_leaves: b("spruce_leaves"),
            short_grass: b("short_grass"),
            poppy: b("poppy"),
            dandelion: b("dandelion"),
            cactus: b("cactus"),
            coal_ore: b("coal_ore"),
            iron_ore: b("iron_ore"),
            copper_ore: b("copper_ore"),
            gold_ore: b("gold_ore"),
            redstone_ore: b("redstone_ore"),
            diamond_ore: b("diamond_ore"),
            deepslate_iron_ore: b("deepslate_iron_ore"),
            deepslate_gold_ore: b("deepslate_gold_ore"),
            deepslate_redstone_ore: b("deepslate_redstone_ore"),
            deepslate_diamond_ore: b("deepslate_diamond_ore"),
            biome_deep_ocean: bio("minecraft:deep_ocean"),
            biome_ocean: bio("minecraft:ocean"),
            biome_beach: bio("minecraft:beach"),
            biome_desert: bio("minecraft:desert"),
            biome_plains: bio("minecraft:plains"),
            biome_forest: bio("minecraft:forest"),
            biome_taiga: bio("minecraft:taiga"),
            biome_mountains: bio("minecraft:windswept_hills"),
            biome_snowy_peaks: bio("minecraft:snowy_slopes"),
        }
    }

    fn biome_id(&self, kind: BiomeKind) -> i32 {
        match kind {
            BiomeKind::DeepOcean => self.biome_deep_ocean,
            BiomeKind::Ocean => self.biome_ocean,
            BiomeKind::Beach => self.biome_beach,
            BiomeKind::Desert => self.biome_desert,
            BiomeKind::Plains => self.biome_plains,
            BiomeKind::Forest => self.biome_forest,
            BiomeKind::Taiga => self.biome_taiga,
            BiomeKind::Mountains => self.biome_mountains,
            BiomeKind::SnowyPeaks => self.biome_snowy_peaks,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OverworldGenerator {
    seed: i64,
    palette: Palette,
    spawn_position: [f64; 3],
}

impl OverworldGenerator {
    pub fn new(seed: i64) -> Self {
        let palette = Palette::load();
        let mut generator = Self {
            seed,
            palette,
            spawn_position: [0.5, 80.0, 0.5],
        };
        generator.spawn_position = generator.find_safe_spawn();
        generator
    }

    pub fn seed(&self) -> i64 {
        self.seed
    }

    fn find_safe_spawn(&self) -> [f64; 3] {
        // Spiral outward from (0, 0) in 4-block steps looking for dry land above sea level.
        for radius in (0..=256i32).step_by(4) {
            for (x, z) in [
                (radius, 0),
                (-radius, 0),
                (0, radius),
                (0, -radius),
                (radius, radius),
                (-radius, -radius),
                (radius, -radius),
                (-radius, radius),
            ] {
                let (height, biome) = self.sample_column(x, z);
                if height >= SEA_LEVEL && !matches!(biome, BiomeKind::Ocean | BiomeKind::DeepOcean)
                {
                    return [
                        f64::from(x) + 0.5,
                        f64::from(height + 2),
                        f64::from(z) + 0.5,
                    ];
                }
            }
        }
        let (height, _) = self.sample_column(0, 0);
        [0.5, f64::from(height.max(SEA_LEVEL) + 2), 0.5]
    }

    fn sample_column(&self, wx: i32, wz: i32) -> (i32, BiomeKind) {
        let s = self.seed as u64;
        let x = f64::from(wx);
        let z = f64::from(wz);

        let continental = fbm2(x * 0.0022, z * 0.0022, s ^ 0x9e37_79b9_7f4a_7c15, 4);
        let erosion = fbm2(x * 0.0045, z * 0.0045, s ^ 0xbf58_476d_1ce4_e5b9, 3);
        let ridges = fbm2(x * 0.009, z * 0.009, s ^ 0x94d0_49bb_1331_11eb, 4);
        let temp = fbm2(x * 0.0018, z * 0.0018, s ^ 0x243f_6a88_85a3_08d3, 3);
        let humid = fbm2(x * 0.0018, z * 0.0018, s ^ 0x1319_8a2e_0370_7344, 3);

        // Base elevation from continentalness + mountain amplification.
        let mountain_factor =
            ((continental - 0.12).max(0.0) * (1.0 - erosion.clamp(-1.0, 1.0)) * 0.7)
                .clamp(0.0, 1.5);
        let height_f =
            64.0 + continental * 34.0 + ridges * (10.0 + mountain_factor * 42.0) + erosion * 6.0;
        let height = (height_f.round() as i32).clamp(MIN_Y + 8, MIN_Y + HEIGHT - 24);

        let biome = if height < SEA_LEVEL - 12 {
            BiomeKind::DeepOcean
        } else if height < SEA_LEVEL - 1 {
            BiomeKind::Ocean
        } else if height <= SEA_LEVEL + 2 && continental < 0.12 {
            BiomeKind::Beach
        } else if height >= 112 {
            BiomeKind::SnowyPeaks
        } else if height >= 94 {
            BiomeKind::Mountains
        } else if temp > 0.25 && humid < -0.05 {
            BiomeKind::Desert
        } else if temp < -0.2 {
            BiomeKind::Taiga
        } else if humid > 0.1 {
            BiomeKind::Forest
        } else {
            BiomeKind::Plains
        };

        (height, biome)
    }

    fn is_cave(&self, wx: i32, wy: i32, wz: i32, surface_y: i32) -> bool {
        if wy >= surface_y - 5 || wy <= MIN_Y + 4 {
            return false;
        }
        let s = self.seed as u64;
        let n1 = value_noise3(
            f64::from(wx) * 0.045,
            f64::from(wy) * 0.065,
            f64::from(wz) * 0.045,
            s ^ 0x517c_c1b7_2722_0a95,
        );
        let n2 = value_noise3(
            f64::from(wx) * 0.045,
            f64::from(wy) * 0.065,
            f64::from(wz) * 0.045,
            s ^ 0x85eb_ca6b_c2b2_ae35,
        );
        (n1 * n1 + n2 * n2) < 0.032
    }

    fn pick_ore(&self, wx: i32, wy: i32, wz: i32, deepslate: bool) -> Option<i32> {
        let h = hash3(wx, wy, wz, self.seed as u64 ^ 0xd6e8_feb8_6659_fd93);
        let roll = (h & 0xfff) as u32;
        if deepslate {
            if wy <= -16 && roll < 7 {
                return Some(self.palette.deepslate_diamond_ore);
            }
            if wy <= -8 && roll < 18 {
                return Some(self.palette.deepslate_redstone_ore);
            }
            if roll < 28 {
                return Some(self.palette.deepslate_gold_ore);
            }
            if roll < 52 {
                return Some(self.palette.deepslate_iron_ore);
            }
        } else {
            if wy <= 16 && roll < 4 {
                return Some(self.palette.diamond_ore);
            }
            if wy <= 15 && roll < 12 {
                return Some(self.palette.redstone_ore);
            }
            if wy <= 32 && roll < 20 {
                return Some(self.palette.gold_ore);
            }
            if wy <= 64 && roll < 48 {
                return Some(self.palette.iron_ore);
            }
            if (0..=96).contains(&wy) && roll < 76 {
                return Some(self.palette.copper_ore);
            }
            if wy >= 0 && roll < 115 {
                return Some(self.palette.coal_ore);
            }
        }
        None
    }

    fn place_tree(
        &self,
        chunk: &mut Chunk,
        lx: usize,
        base_y: i32,
        lz: usize,
        kind: BiomeKind,
        h: u64,
    ) {
        let (log, leaves, height) = match kind {
            BiomeKind::Taiga => (
                self.palette.spruce_log,
                self.palette.spruce_leaves,
                6 + (h % 2) as i32,
            ),
            BiomeKind::Forest if (h >> 8).is_multiple_of(3) => (
                self.palette.birch_log,
                self.palette.birch_leaves,
                5 + (h % 2) as i32,
            ),
            _ => (
                self.palette.oak_log,
                self.palette.oak_leaves,
                4 + (h % 3) as i32,
            ),
        };
        if base_y + height + 2 >= MIN_Y + HEIGHT {
            return;
        }
        // Replace grass beneath trunk with dirt.
        chunk.set_block(lx, base_y - 1, lz, self.palette.dirt);
        // Trunk.
        for dy in 0..height {
            chunk.set_block(lx, base_y + dy, lz, log);
        }
        // Leaf canopy (kept within +-2 blocks, safe because lx/lz in 2..14).
        let top = base_y + height;
        for dy in -2..=1i32 {
            let radius: i32 = if dy >= 0 { 1 } else { 2 };
            for dz in -radius..=radius {
                for dx in -radius..=radius {
                    if dy == 1 && dx.abs() + dz.abs() > 1 {
                        continue;
                    }
                    if dy < 0 && dx.abs() == radius && dz.abs() == radius && (h & 1) == 0 {
                        continue;
                    }
                    let tx = (lx as i32 + dx) as usize;
                    let tz = (lz as i32 + dz) as usize;
                    let ty = top + dy;
                    if chunk.get_block(tx, ty, tz) == self.palette.air {
                        chunk.set_block(tx, ty, tz, leaves);
                    }
                }
            }
        }
    }
}

impl ChunkGenerator for OverworldGenerator {
    fn generate_chunk(&self, cx: i32, cz: i32) -> Arc<Chunk> {
        let mut heights = [[0i32; 16]; 16];
        let mut biomes = [[BiomeKind::Plains; 16]; 16];

        for lz in 0..16 {
            let wz = cz * 16 + lz as i32;
            for lx in 0..16 {
                let wx = cx * 16 + lx as i32;
                let (h, b) = self.sample_column(wx, wz);
                heights[lz][lx] = h;
                biomes[lz][lx] = b;
            }
        }

        let sections: Box<[ChunkSection; SECTION_COUNT]> = Box::new(std::array::from_fn(|_| {
            ChunkSection::empty(self.palette.biome_plains)
        }));
        let mut chunk = Chunk::from_sections(sections);

        // Assign 4x4x4 biome cells across all sections from the column biomes.
        for sec_idx in 0..SECTION_COUNT {
            let sec = chunk.section_mut(sec_idx);
            for bz in 0..4 {
                for bx in 0..4 {
                    let bid = self.palette.biome_id(biomes[bz * 4][bx * 4]);
                    for by in 0..4 {
                        sec.set_biome(bx, by, bz, bid);
                    }
                }
            }
        }

        // Fill terrain columns.
        for lz in 0..16 {
            let wz = cz * 16 + lz as i32;
            for lx in 0..16 {
                let wx = cx * 16 + lx as i32;
                let surface = heights[lz][lx];
                let biome = biomes[lz][lx];
                let max_y = surface.max(SEA_LEVEL - 1);

                for wy in MIN_Y..=max_y {
                    let state = if wy == MIN_Y
                        || (wy <= MIN_Y + 4
                            && ((hash3(wx, wy, wz, self.seed as u64) % 5) as i32)
                                < (MIN_Y + 5 - wy))
                    {
                        self.palette.bedrock
                    } else if wy > surface {
                        self.palette.water
                    } else if self.is_cave(wx, wy, wz, surface) {
                        if wy <= MIN_Y + 10 {
                            self.palette.lava
                        } else {
                            self.palette.air
                        }
                    } else if wy == surface {
                        match biome {
                            BiomeKind::Desert | BiomeKind::Beach => self.palette.sand,
                            BiomeKind::Ocean | BiomeKind::DeepOcean => {
                                if (hash3(wx, wy, wz, self.seed as u64) & 3) == 0 {
                                    self.palette.gravel
                                } else {
                                    self.palette.sand
                                }
                            }
                            BiomeKind::SnowyPeaks => self.palette.snow_block,
                            BiomeKind::Mountains => self.palette.stone,
                            BiomeKind::Plains | BiomeKind::Forest | BiomeKind::Taiga => {
                                if surface >= SEA_LEVEL {
                                    self.palette.grass_block
                                } else {
                                    self.palette.dirt
                                }
                            }
                        }
                    } else if wy >= surface - 3 {
                        match biome {
                            BiomeKind::Desert => self.palette.sandstone,
                            BiomeKind::Beach => self.palette.sand,
                            BiomeKind::Mountains | BiomeKind::SnowyPeaks => self.palette.stone,
                            _ => self.palette.dirt,
                        }
                    } else {
                        let is_deep = wy < 0;
                        if let Some(ore) = self.pick_ore(wx, wy, wz, is_deep) {
                            ore
                        } else if is_deep {
                            self.palette.deepslate
                        } else {
                            self.palette.stone
                        }
                    };
                    if state != self.palette.air {
                        chunk.set_block(lx, wy, lz, state);
                    }
                }
            }
        }

        // Surface vegetation & chunk-interior trees (lx, lz in 2..14 so canopies never clip).
        for lz in 2..14 {
            let wz = cz * 16 + lz as i32;
            for lx in 2..14 {
                let wx = cx * 16 + lx as i32;
                let surface = heights[lz][lx];
                let biome = biomes[lz][lx];
                if surface < SEA_LEVEL || surface + 8 >= MIN_Y + HEIGHT {
                    continue;
                }
                let h = hash3(wx, surface, wz, self.seed as u64 ^ 0x7f4a_7c15_9e37_79b9);
                match biome {
                    BiomeKind::Forest => {
                        if h.is_multiple_of(43) && lx.is_multiple_of(3) && lz.is_multiple_of(3) {
                            self.place_tree(&mut chunk, lx, surface + 1, lz, biome, h);
                        } else if h.is_multiple_of(11) {
                            chunk.set_block(lx, surface + 1, lz, self.palette.short_grass);
                        }
                    }
                    BiomeKind::Taiga
                        if h.is_multiple_of(53) && lx.is_multiple_of(3) && lz.is_multiple_of(3) =>
                    {
                        self.place_tree(&mut chunk, lx, surface + 1, lz, biome, h);
                    }
                    BiomeKind::Plains => {
                        if h.is_multiple_of(211) && lx.is_multiple_of(4) && lz.is_multiple_of(4) {
                            self.place_tree(&mut chunk, lx, surface + 1, lz, biome, h);
                        } else if h.is_multiple_of(7) {
                            chunk.set_block(lx, surface + 1, lz, self.palette.short_grass);
                        } else if h.is_multiple_of(47) {
                            let flower = if (h & 1) == 0 {
                                self.palette.poppy
                            } else {
                                self.palette.dandelion
                            };
                            chunk.set_block(lx, surface + 1, lz, flower);
                        }
                    }
                    BiomeKind::Desert if h.is_multiple_of(127) => {
                        let cactus_h = 1 + (h % 3) as i32;
                        for dy in 1..=cactus_h {
                            chunk.set_block(lx, surface + dy, lz, self.palette.cactus);
                        }
                    }
                    _ => {}
                }
            }
        }

        Arc::new(chunk)
    }

    fn spawn_position(&self) -> [f64; 3] {
        self.spawn_position
    }

    fn descriptor(&self) -> Option<super::GeneratorSpec> {
        Some(super::GeneratorSpec::overworld(self.seed))
    }
}

#[inline]
fn fade(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[inline]
fn hash2(x: i32, z: i32, seed: u64) -> u64 {
    let mut h = seed
        .wrapping_add((x as i64 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .wrapping_add((z as i64 as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9));
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

#[inline]
fn hash3(x: i32, y: i32, z: i32, seed: u64) -> u64 {
    let mut h = seed
        .wrapping_add((x as i64 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .wrapping_add((y as i64 as u64).wrapping_mul(0x94d0_49bb_1331_11eb))
        .wrapping_add((z as i64 as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9));
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

#[inline]
fn grad2(h: u64, dx: f64, dz: f64) -> f64 {
    match h & 7 {
        0 => dx + dz,
        1 => -dx + dz,
        2 => dx - dz,
        3 => -dx - dz,
        4 => dx,
        5 => -dx,
        6 => dz,
        _ => -dz,
    }
}

fn perlin2(x: f64, z: f64, seed: u64) -> f64 {
    let xi = x.floor() as i32;
    let zi = z.floor() as i32;
    let xf = x - f64::from(xi);
    let zf = z - f64::from(zi);
    let u = fade(xf);
    let v = fade(zf);

    let g00 = grad2(hash2(xi, zi, seed), xf, zf);
    let g10 = grad2(hash2(xi + 1, zi, seed), xf - 1.0, zf);
    let g01 = grad2(hash2(xi, zi + 1, seed), xf, zf - 1.0);
    let g11 = grad2(hash2(xi + 1, zi + 1, seed), xf - 1.0, zf - 1.0);

    lerp(lerp(g00, g10, u), lerp(g01, g11, u), v)
}

fn fbm2(x: f64, z: f64, seed: u64, octaves: u32) -> f64 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut freq = 1.0;
    let mut norm = 0.0;
    for i in 0..octaves {
        sum += perlin2(x * freq, z * freq, seed.wrapping_add(u64::from(i) * 1013)) * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

#[inline]
fn unit_val(h: u64) -> f64 {
    ((h >> 11) as f64) * (1.0 / ((1u64 << 53) as f64)) * 2.0 - 1.0
}

fn value_noise3(x: f64, y: f64, z: f64, seed: u64) -> f64 {
    let xi = x.floor() as i32;
    let yi = y.floor() as i32;
    let zi = z.floor() as i32;
    let u = fade(x - f64::from(xi));
    let v = fade(y - f64::from(yi));
    let w = fade(z - f64::from(zi));

    let c000 = unit_val(hash3(xi, yi, zi, seed));
    let c100 = unit_val(hash3(xi + 1, yi, zi, seed));
    let c010 = unit_val(hash3(xi, yi + 1, zi, seed));
    let c110 = unit_val(hash3(xi + 1, yi + 1, zi, seed));
    let c001 = unit_val(hash3(xi, yi, zi + 1, seed));
    let c101 = unit_val(hash3(xi + 1, yi, zi + 1, seed));
    let c011 = unit_val(hash3(xi, yi + 1, zi + 1, seed));
    let c111 = unit_val(hash3(xi + 1, yi + 1, zi + 1, seed));

    let x00 = lerp(c000, c100, u);
    let x10 = lerp(c010, c110, u);
    let x01 = lerp(c001, c101, u);
    let x11 = lerp(c011, c111, u);
    lerp(lerp(x00, x10, v), lerp(x01, x11, v), w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overworld_is_deterministic_stratified_and_has_safe_spawn() {
        let gen_a = OverworldGenerator::new(2026);
        let gen_b = OverworldGenerator::new(2026);
        let gen_c = OverworldGenerator::new(9999);

        let c_a = gen_a.generate_chunk(0, 0);
        let c_b = gen_b.generate_chunk(0, 0);
        let c_c = gen_c.generate_chunk(0, 0);
        assert_eq!(c_a, c_b);
        assert_ne!(c_a, c_c);

        // Bedrock at y = -64 across the entire chunk.
        for lz in 0..16 {
            for lx in 0..16 {
                assert_eq!(c_a.get_block(lx, MIN_Y, lz), crate::game::BEDROCK_STATE);
            }
        }

        // Spawn must be on or above sea level.
        let spawn = gen_a.spawn_position();
        assert!(spawn[1] >= f64::from(SEA_LEVEL));
    }
}
