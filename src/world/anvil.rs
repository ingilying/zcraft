//! Vanilla Minecraft Java 1.21.1 (DataVersion 3955) Anvil persistence:
//! - `level.dat`: GZip-compressed NBT world metadata & generator settings
//! - `region/r.<rx>.<rz>.mca`: 32x32 chunk region files (lazy single-chunk load & atomic batch save)
//! - `playerdata/<uuid>.dat`: GZip-compressed NBT per-player state & inventory
mod chunk_codec;
mod playerdata;
mod region;

pub use playerdata::SavedPlayer;

use super::{
    MIN_Y,
    clock::PersistedClock,
    generator::{FlatGeneratorConfig, FlatLayerConfig},
    nbt::{Compound, Tag, read_gzip_nbt, write_gzip_nbt_atomic},
};
use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const DATA_VERSION_1_21_1: i32 = 3955;
const ANVIL_VERSION: i32 = 19133;

/// Loaded `level.dat` snapshot.
#[derive(Debug, Clone)]
pub struct LoadedLevel {
    pub clock: PersistedClock,
    pub generator: Option<super::GeneratorSpec>,
    pub spawn: Option<[f64; 3]>,
}

/// Anvil directory handle (`<world_dir>`).
#[derive(Debug, Clone)]
pub struct AnvilStorage {
    root: PathBuf,
}

impl AnvilStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn level_dat_path(&self) -> PathBuf {
        self.root.join("level.dat")
    }

    pub fn load_level(&self) -> io::Result<Option<LoadedLevel>> {
        let path = self.level_dat_path();
        if !path.exists() {
            return Ok(None);
        }
        let (_, root) = read_gzip_nbt(&path)?;
        let data = root
            .get("Data")
            .and_then(Tag::as_compound)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "level.dat missing Data"))?;

        let clock = PersistedClock {
            world_age: data.get("Time").and_then(Tag::as_long).unwrap_or(0),
            day_time: data
                .get("DayTime")
                .and_then(Tag::as_long)
                .unwrap_or(super::INITIAL_DAY_TIME),
            clear_weather_time: data
                .get("clearWeatherTime")
                .and_then(Tag::as_int)
                .unwrap_or(0)
                .max(0) as u32,
            raining: data.get("raining").and_then(Tag::as_byte).unwrap_or(0) != 0,
            rain_time: data
                .get("rainTime")
                .and_then(Tag::as_int)
                .unwrap_or(12_000)
                .max(1) as u32,
            thundering: data.get("thundering").and_then(Tag::as_byte).unwrap_or(0) != 0,
            thunder_time: data
                .get("thunderTime")
                .and_then(Tag::as_int)
                .unwrap_or(12_000)
                .max(1) as u32,
        };

        let spawn = match (
            data.get("SpawnX").and_then(Tag::as_int),
            data.get("SpawnY").and_then(Tag::as_int),
            data.get("SpawnZ").and_then(Tag::as_int),
        ) {
            (Some(x), Some(y), Some(z)) => {
                Some([f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5])
            }
            _ => None,
        };

        let generator = parse_generator_settings(data);
        Ok(Some(LoadedLevel {
            clock,
            generator,
            spawn,
        }))
    }

    pub fn save_level(
        &self,
        clock: PersistedClock,
        generator: Option<&super::GeneratorSpec>,
        spawn: [f64; 3],
    ) -> io::Result<()> {
        let level_name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("world")
            .to_string();

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);

        let mut version_tag = Compound::new();
        version_tag.insert("Id".into(), Tag::Int(DATA_VERSION_1_21_1));
        version_tag.insert("Name".into(), Tag::String("1.21.1".into()));
        version_tag.insert("Series".into(), Tag::String("main".into()));
        version_tag.insert("Snapshot".into(), Tag::Byte(0));

        let mut game_rules = Compound::new();
        game_rules.insert("spawnChunkRadius".into(), Tag::String("2".into()));
        game_rules.insert(
            "commandModificationBlockLimit".into(),
            Tag::String("32768".into()),
        );
        game_rules.insert("keepInventory".into(), Tag::String("false".into()));

        let mut data = Compound::new();
        data.insert("DataVersion".into(), Tag::Int(DATA_VERSION_1_21_1));
        data.insert("Version".into(), Tag::Compound(version_tag));
        data.insert("version".into(), Tag::Int(ANVIL_VERSION));
        data.insert("LevelName".into(), Tag::String(level_name));
        data.insert("GameType".into(), Tag::Int(0));
        data.insert("Difficulty".into(), Tag::Byte(2)); // Normal
        data.insert("DifficultyLocked".into(), Tag::Byte(0));
        data.insert("hardcore".into(), Tag::Byte(0));
        data.insert("allowCommands".into(), Tag::Byte(1));
        data.insert("WasModded".into(), Tag::Byte(0));
        data.insert(
            "ServerBrands".into(),
            Tag::List(vec![Tag::String("vanilla".into())]),
        );
        data.insert("GameRules".into(), Tag::Compound(game_rules));
        data.insert("Time".into(), Tag::Long(clock.world_age));
        data.insert("DayTime".into(), Tag::Long(clock.day_time));
        data.insert(
            "clearWeatherTime".into(),
            Tag::Int(clock.clear_weather_time as i32),
        );
        data.insert("raining".into(), Tag::Byte(i8::from(clock.raining)));
        data.insert("rainTime".into(), Tag::Int(clock.rain_time as i32));
        data.insert("thundering".into(), Tag::Byte(i8::from(clock.thundering)));
        data.insert("thunderTime".into(), Tag::Int(clock.thunder_time as i32));
        data.insert("SpawnX".into(), Tag::Int(spawn[0].floor() as i32));
        data.insert("SpawnY".into(), Tag::Int(spawn[1].floor() as i32));
        data.insert("SpawnZ".into(), Tag::Int(spawn[2].floor() as i32));
        data.insert("SpawnAngle".into(), Tag::Float(0.0));
        data.insert("LastPlayed".into(), Tag::Long(now_ms));
        data.insert("initialized".into(), Tag::Byte(1));

        let gen_compound = generator.map(|spec| {
            if spec.id == "minecraft:flat"
                && let Ok(flat_cfg) =
                    serde_json::from_value::<FlatGeneratorConfig>(spec.settings.clone())
            {
                let mut layers = Vec::with_capacity(flat_cfg.layers.len());
                for layer in &flat_cfg.layers {
                    let mut lc = Compound::new();
                    lc.insert("block".into(), Tag::String(layer.block.clone()));
                    lc.insert("height".into(), Tag::Int(i32::from(layer.height)));
                    layers.push(Tag::Compound(lc));
                }
                let mut settings = Compound::new();
                settings.insert("biome".into(), Tag::String(flat_cfg.biome.clone()));
                settings.insert("layers".into(), Tag::List(layers));
                settings.insert("zcraft_base_y".into(), Tag::Int(flat_cfg.base_y));

                let mut gc = Compound::new();
                gc.insert("type".into(), Tag::String("minecraft:flat".into()));
                gc.insert("settings".into(), Tag::Compound(settings));
                (spec.seed, gc)
            } else if spec.id == "minecraft:noise" {
                let mut gc = Compound::new();
                gc.insert("type".into(), Tag::String("minecraft:noise".into()));
                let preset = spec
                    .settings
                    .as_str()
                    .unwrap_or("minecraft:overworld")
                    .to_string();
                gc.insert("settings".into(), Tag::String(preset));
                (spec.seed, gc)
            } else {
                let mut gc = Compound::new();
                gc.insert("type".into(), Tag::String(spec.id.clone()));
                if !spec.settings.is_null() {
                    gc.insert(
                        "zcraft_settings".into(),
                        Tag::String(spec.settings.to_string()),
                    );
                }
                (spec.seed, gc)
            }
        });

        if let Some((seed, gen_compound)) = gen_compound {
            let mut overworld = Compound::new();
            overworld.insert("type".into(), Tag::String("minecraft:overworld".into()));
            overworld.insert("generator".into(), Tag::Compound(gen_compound));

            let mut dimensions = Compound::new();
            dimensions.insert("minecraft:overworld".into(), Tag::Compound(overworld));

            let mut world_gen = Compound::new();
            world_gen.insert("bonus_chest".into(), Tag::Byte(0));
            world_gen.insert("generate_features".into(), Tag::Byte(1));
            world_gen.insert("seed".into(), Tag::Long(seed));
            world_gen.insert("dimensions".into(), Tag::Compound(dimensions));
            data.insert("WorldGenSettings".into(), Tag::Compound(world_gen));
        }

        let mut root = Compound::new();
        root.insert("Data".into(), Tag::Compound(data));
        write_gzip_nbt_atomic(&self.level_dat_path(), "", &root)
    }
}

fn parse_generator_settings(data: &Compound) -> Option<super::GeneratorSpec> {
    let world_gen = data.get("WorldGenSettings")?.as_compound()?;
    let seed = world_gen.get("seed").and_then(Tag::as_long).unwrap_or(0);
    let generator = world_gen
        .get("dimensions")?
        .as_compound()?
        .get("minecraft:overworld")?
        .as_compound()?
        .get("generator")?
        .as_compound()?;
    match generator.get("type")?.as_str()? {
        "minecraft:noise" => Some(super::GeneratorSpec::overworld(seed)),
        "minecraft:flat" => {
            let settings = generator.get("settings")?.as_compound()?;
            let biome = settings
                .get("biome")
                .and_then(Tag::as_str)
                .unwrap_or("minecraft:plains")
                .to_string();
            let base_y = settings
                .get("zcraft_base_y")
                .and_then(Tag::as_int)
                .unwrap_or(MIN_Y);
            let raw_layers = settings.get("layers")?.as_list()?;
            let mut layers = Vec::with_capacity(raw_layers.len());
            for entry in raw_layers {
                let map = entry.as_compound()?;
                let block = map.get("block")?.as_str()?.to_string();
                let height = u16::try_from(map.get("height")?.as_int()?).ok()?;
                layers.push(FlatLayerConfig { block, height });
            }
            let cfg = FlatGeneratorConfig {
                base_y,
                biome,
                layers,
            };
            cfg.validate().ok()?;
            Some(super::GeneratorSpec::flat(&cfg))
        }
        other => {
            let settings = generator
                .get("zcraft_settings")
                .and_then(Tag::as_str)
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or(serde_json::Value::Null);
            Some(super::GeneratorSpec {
                id: other.to_string(),
                seed,
                settings,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::GameMode,
        game::{Inventory, Item, ItemStack, Player, registry},
        registry_data::biome_id,
        world::{ChunkGenerator, FlatGenerator},
    };
    use std::{collections::BTreeMap, sync::Arc};
    use uuid::Uuid;

    #[test]
    fn level_dat_region_mca_and_playerdata_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = AnvilStorage::new(dir.path());
        let generator = FlatGenerator::default();

        // 1. level.dat
        let clock = PersistedClock {
            world_age: 12_345,
            day_time: 18_000,
            clear_weather_time: 0,
            raining: true,
            rain_time: 5_000,
            thundering: false,
            thunder_time: 9_000,
        };
        let spec = crate::world::GeneratorSpec::flat(generator.config());
        storage
            .save_level(clock, Some(&spec), [8.5, 65.0, 8.5])
            .unwrap();
        let loaded_level = storage.load_level().unwrap().unwrap();
        assert_eq!(loaded_level.clock, clock);
        assert_eq!(loaded_level.generator, Some(spec));

        // 2. Lazy single-chunk load/save across regions with block state properties & biomes
        let mut c0 = (*generator.generate_chunk(0, 0)).clone();
        let mut props = BTreeMap::new();
        props.insert("facing".into(), "east".into());
        let stairs_state = registry()
            .resolve_state("minecraft:oak_stairs", &props)
            .unwrap();
        c0.set_block(3, 64, 5, stairs_state);
        c0.set_block(8, 60, 8, crate::game::AIR_STATE);
        let desert = biome_id("minecraft:desert").unwrap();
        c0.section_mut(7).set_biome(1, 2, 3, desert);

        let mut c_neg = (*generator.generate_chunk(-1, 0)).clone();
        c_neg.set_block(14, 65, 1, crate::game::GLASS_STATE);

        let c0 = Arc::new(c0);
        let c_neg = Arc::new(c_neg);

        let saved = storage
            .save_chunks(&[([0, 0], c0.clone()), ([-1, 0], c_neg.clone())])
            .unwrap();
        assert_eq!(saved, 2);
        assert!(dir.path().join("region/r.0.0.mca").exists());
        assert!(dir.path().join("region/r.-1.0.mca").exists());

        assert_eq!(
            storage.load_chunk(0, 0).unwrap().as_ref(),
            Some(c0.as_ref())
        );
        assert_eq!(
            storage.load_chunk(-1, 0).unwrap().as_ref(),
            Some(c_neg.as_ref())
        );
        assert!(storage.load_chunk(5, 5).unwrap().is_none());

        // 3. playerdata/<uuid>.dat
        let uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let mut player = Player::at_spawn(1, uuid, "Steve".into(), GameMode::Creative);
        player.position = [12.25, 70.5, -4.75];
        player.yaw = 90.0;
        player.pitch = -15.5;
        player.on_ground = true;
        let mut inv = Inventory::default();
        inv.select(3);
        let pickaxe = Item::by_name("iron_pickaxe").unwrap();
        inv.set_creative(45, ItemStack::with_damage(pickaxe, 1, 17));

        storage.save_player(&player, &inv).unwrap();
        let loaded_player = storage.load_player(uuid).unwrap().unwrap();
        assert_eq!(loaded_player.position, player.position);
        assert_eq!(loaded_player.yaw, player.yaw);
        assert_eq!(loaded_player.pitch, player.pitch);
        assert!(loaded_player.on_ground);
        assert_eq!(loaded_player.game_mode, GameMode::Creative);
        assert_eq!(loaded_player.inventory.selected(), 3);
        assert_eq!(
            loaded_player.inventory.slots()[45],
            ItemStack::with_damage(pickaxe, 1, 17)
        );
    }
}
