//! Vanilla Minecraft Java 1.21.1 (DataVersion 3955) Anvil persistence:
//! - `level.dat`: GZip-compressed NBT world metadata & generator settings
//! - `region/r.<rx>.<rz>.mca`: 32x32 chunk region files (lazy single-chunk load & atomic batch save)
//! - `playerdata/<uuid>.dat`: GZip-compressed NBT per-player state & inventory
use super::{
    BiomeStorage, BlockEntity, BlockStorage, CHEST_SLOTS, Chunk, ChunkSection, FurnaceData, MIN_Y,
    PLAINS_BIOME_ID, SECTION_COUNT,
    clock::PersistedClock,
    generator::{FlatGeneratorConfig, FlatLayerConfig},
    nbt::{
        Compound, Tag, read_gzip_nbt, read_named_nbt, write_bytes_atomic, write_gzip_nbt_atomic,
        write_named_nbt,
    },
};
use crate::{
    config::GameMode,
    game::{Inventory, Item, ItemStack, Player, SLOT_COUNT, registry},
    registry_data::{biome_id, biome_name},
};
use flate2::{
    Compression,
    read::{GzDecoder, ZlibDecoder},
    write::ZlibEncoder,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const DATA_VERSION_1_21_1: i32 = 3955;
const ANVIL_VERSION: i32 = 19133;
const SECTOR_BYTES: usize = 4096;
const REGION_CHUNKS: usize = 1024;
const HEADER_BYTES: usize = SECTOR_BYTES * 2;
const LOWEST_SECTION_Y: i32 = MIN_Y / 16; // -4

/// Loaded `level.dat` snapshot.
#[derive(Debug, Clone)]
pub struct LoadedLevel {
    pub clock: PersistedClock,
    pub generator: Option<super::GeneratorSpec>,
    pub spawn: Option<[f64; 3]>,
}

/// Persisted player state restored from `playerdata/<uuid>.dat`.
#[derive(Debug, Clone)]
pub struct SavedPlayer {
    pub position: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub game_mode: GameMode,
    pub health: f32,
    pub food: i32,
    pub saturation: f32,
    pub respawn_point: Option<super::BlockPos>,
    pub inventory: Inventory,
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

    fn region_dir(&self) -> PathBuf {
        self.root.join("region")
    }

    fn region_path(&self, rx: i32, rz: i32) -> PathBuf {
        self.region_dir().join(format!("r.{rx}.{rz}.mca"))
    }

    fn player_path(&self, uuid: Uuid) -> PathBuf {
        self.root.join("playerdata").join(format!("{uuid}.dat"))
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

    /// Lazily load a single chunk from its `region/r.<rx>.<rz>.mca` file.
    /// Returns `Ok(None)` if the region file or chunk slot does not exist.
    pub fn load_chunk(&self, cx: i32, cz: i32) -> io::Result<Option<Chunk>> {
        let rx = cx.div_euclid(32);
        let rz = cz.div_euclid(32);
        let path = self.region_path(rx, rz);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        if bytes.len() < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated Anvil region header",
            ));
        }
        let slot = region_slot(cx, cz);
        let loc = u32::from_be_bytes(bytes[slot * 4..slot * 4 + 4].try_into().unwrap());
        let offset = (loc >> 8) as usize;
        let sectors = (loc & 0xff) as usize;
        if offset == 0 || sectors == 0 {
            return Ok(None);
        }
        let start = offset * SECTOR_BYTES;
        let end_sector = start + sectors * SECTOR_BYTES;
        if offset < 2 || end_sector > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Anvil chunk sector out of bounds",
            ));
        }
        let length = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
        if length == 0 || start + 4 + length > end_sector {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Anvil chunk length",
            ));
        }
        let payload = &bytes[start + 4..start + 4 + length];
        let chunk = decode_chunk_anvil(payload)?;
        Ok(Some(chunk))
    }

    /// Save a batch of dirty chunks grouped by region file (`r.<rx>.<rz>.mca`).
    pub fn save_chunks(&self, chunks: &[([i32; 2], Arc<Chunk>)]) -> io::Result<usize> {
        if chunks.is_empty() {
            return Ok(0);
        }
        type RegionUpdate<'a> = ([i32; 2], &'a Chunk);
        let mut by_region: BTreeMap<[i32; 2], Vec<RegionUpdate<'_>>> = BTreeMap::new();
        for (coords, chunk) in chunks {
            let region = [coords[0].div_euclid(32), coords[1].div_euclid(32)];
            by_region
                .entry(region)
                .or_default()
                .push((*coords, chunk.as_ref()));
        }
        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        let mut saved = 0usize;
        for ([rx, rz], updates) in by_region {
            let path = self.region_path(rx, rz);
            let mut region = if path.exists() {
                RegionFile::parse(&fs::read(&path)?)?
            } else {
                RegionFile::default()
            };
            for ([cx, cz], chunk) in updates {
                let slot = region_slot(cx, cz);
                let payload = encode_chunk_anvil(cx, cz, chunk)?;
                region.chunks[slot] = Some((payload, now_sec));
                saved += 1;
            }
            let encoded = region.encode();
            write_bytes_atomic(&path, &encoded)?;
        }
        Ok(saved)
    }

    pub fn load_player(&self, uuid: Uuid) -> io::Result<Option<SavedPlayer>> {
        let path = self.player_path(uuid);
        if !path.exists() {
            return Ok(None);
        }
        let (_, root) = read_gzip_nbt(&path)?;
        let pos_list = root.get("Pos").and_then(Tag::as_list);
        let position = match pos_list {
            Some([a, b, c]) => [
                a.as_double().unwrap_or(super::SPAWN_X),
                b.as_double().unwrap_or(super::SPAWN_Y),
                c.as_double().unwrap_or(super::SPAWN_Z),
            ],
            _ => [super::SPAWN_X, super::SPAWN_Y, super::SPAWN_Z],
        };
        let rot_list = root.get("Rotation").and_then(Tag::as_list);
        let (yaw, pitch) = match rot_list {
            Some([y, p]) => (y.as_float().unwrap_or(0.0), p.as_float().unwrap_or(0.0)),
            _ => (0.0, 0.0),
        };
        let on_ground = root.get("OnGround").and_then(Tag::as_byte).unwrap_or(0) != 0;
        let game_mode = match root.get("playerGameType").and_then(Tag::as_int) {
            Some(0) => GameMode::Survival,
            Some(1) => GameMode::Creative,
            _ => GameMode::Spectator,
        };
        let health = root
            .get("Health")
            .and_then(Tag::as_float)
            .unwrap_or(20.0)
            .clamp(0.0, 20.0);
        let food = root
            .get("foodLevel")
            .and_then(Tag::as_int)
            .unwrap_or(20)
            .clamp(0, 20);
        let saturation = root
            .get("foodSaturationLevel")
            .and_then(Tag::as_float)
            .unwrap_or(5.0)
            .clamp(0.0, 20.0);
        let selected = root
            .get("SelectedItemSlot")
            .and_then(Tag::as_int)
            .unwrap_or(0)
            .clamp(0, 8) as u8;

        let mut slots = [None; SLOT_COUNT];
        if let Some(inv) = root.get("Inventory").and_then(Tag::as_list) {
            for item_tag in inv {
                let Some(item_map) = item_tag.as_compound() else {
                    continue;
                };
                let Some(vanilla_slot) = item_map.get("Slot").and_then(Tag::as_byte) else {
                    continue;
                };
                let Some(internal_slot) = vanilla_to_internal_slot(vanilla_slot) else {
                    continue;
                };
                let Some(id_str) = item_map.get("id").and_then(Tag::as_str) else {
                    continue;
                };
                let Some(item) = Item::by_name(id_str) else {
                    continue;
                };
                let count = item_map
                    .get("count")
                    .and_then(Tag::as_int)
                    .or_else(|| item_map.get("Count").and_then(Tag::as_byte).map(i32::from))
                    .unwrap_or(1)
                    .clamp(1, i32::from(item.definition().max_stack))
                    as u8;
                let damage = item_map
                    .get("components")
                    .and_then(Tag::as_compound)
                    .and_then(|c| c.get("minecraft:damage"))
                    .and_then(Tag::as_int)
                    .unwrap_or(0)
                    .max(0) as u32;
                slots[internal_slot] = ItemStack::with_damage(item, count, damage);
            }
        }
        let respawn_point = match (
            root.get("SpawnX").and_then(Tag::as_int),
            root.get("SpawnY").and_then(Tag::as_int),
            root.get("SpawnZ").and_then(Tag::as_int),
        ) {
            (Some(x), Some(y), Some(z)) => Some(super::BlockPos([x, y, z])),
            _ => None,
        };
        let inventory = Inventory::from_persisted(slots, selected);
        Ok(Some(SavedPlayer {
            position,
            yaw,
            pitch,
            on_ground,
            game_mode,
            health,
            food,
            saturation,
            respawn_point,
            inventory,
        }))
    }

    pub fn save_player(&self, player: &Player, inventory: &Inventory) -> io::Result<()> {
        let mut root = Compound::new();
        root.insert("DataVersion".into(), Tag::Int(DATA_VERSION_1_21_1));
        root.insert(
            "Pos".into(),
            Tag::List(vec![
                Tag::Double(player.position[0]),
                Tag::Double(player.position[1]),
                Tag::Double(player.position[2]),
            ]),
        );
        root.insert(
            "Rotation".into(),
            Tag::List(vec![Tag::Float(player.yaw), Tag::Float(player.pitch)]),
        );
        root.insert("OnGround".into(), Tag::Byte(i8::from(player.on_ground)));
        root.insert(
            "playerGameType".into(),
            Tag::Int(i32::from(player.game_mode.wire_id())),
        );
        root.insert("Health".into(), Tag::Float(player.health));
        root.insert("foodLevel".into(), Tag::Int(player.food));
        root.insert("foodSaturationLevel".into(), Tag::Float(player.saturation));
        if let Some(spawn) = player.respawn_point {
            root.insert("SpawnX".into(), Tag::Int(spawn.0[0]));
            root.insert("SpawnY".into(), Tag::Int(spawn.0[1]));
            root.insert("SpawnZ".into(), Tag::Int(spawn.0[2]));
        }
        root.insert(
            "SelectedItemSlot".into(),
            Tag::Int(i32::from(inventory.selected())),
        );

        let mut inv_list = Vec::new();
        for (idx, stack) in inventory.slots().iter().enumerate() {
            let Some(stack) = stack else {
                continue;
            };
            let Some(vanilla_slot) = internal_to_vanilla_slot(idx) else {
                continue;
            };
            let mut item_map = Compound::new();
            item_map.insert("Slot".into(), Tag::Byte(vanilla_slot));
            item_map.insert("id".into(), Tag::String(stack.item.name().into()));
            item_map.insert("count".into(), Tag::Int(i32::from(stack.count)));
            if stack.damage > 0 {
                let mut components = Compound::new();
                components.insert("minecraft:damage".into(), Tag::Int(stack.damage as i32));
                item_map.insert("components".into(), Tag::Compound(components));
            }
            inv_list.push(Tag::Compound(item_map));
        }
        root.insert("Inventory".into(), Tag::List(inv_list));
        write_gzip_nbt_atomic(&self.player_path(player.uuid), "", &root)
    }
}

fn vanilla_to_internal_slot(slot: i8) -> Option<usize> {
    match slot {
        0..=8 => Some(36 + slot as usize),
        9..=35 => Some(slot as usize),
        100 => Some(8), // Boots
        101 => Some(7), // Leggings
        102 => Some(6), // Chestplate
        103 => Some(5), // Helmet
        -106 => Some(45),
        _ => None,
    }
}

fn internal_to_vanilla_slot(slot: usize) -> Option<i8> {
    match slot {
        36..=44 => Some((slot - 36) as i8),
        9..=35 => Some(slot as i8),
        8 => Some(100), // Boots
        7 => Some(101), // Leggings
        6 => Some(102), // Chestplate
        5 => Some(103), // Helmet
        45 => Some(-106),
        _ => None,
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

fn region_slot(cx: i32, cz: i32) -> usize {
    cx.rem_euclid(32) as usize + cz.rem_euclid(32) as usize * 32
}

type RegionSlot = Option<(Vec<u8>, u32)>;

struct RegionFile {
    chunks: Box<[RegionSlot; REGION_CHUNKS]>,
}

impl Default for RegionFile {
    fn default() -> Self {
        Self {
            chunks: Box::new(std::array::from_fn(|_| None)),
        }
    }
}

impl RegionFile {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        if bytes.len() < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated Anvil region header",
            ));
        }
        let mut region = Self::default();
        for slot in 0..REGION_CHUNKS {
            let loc = u32::from_be_bytes(bytes[slot * 4..slot * 4 + 4].try_into().unwrap());
            let offset = (loc >> 8) as usize;
            let sectors = (loc & 0xff) as usize;
            if offset == 0 || sectors == 0 {
                continue;
            }
            let ts = u32::from_be_bytes(
                bytes[SECTOR_BYTES + slot * 4..SECTOR_BYTES + slot * 4 + 4]
                    .try_into()
                    .unwrap(),
            );
            let start = offset * SECTOR_BYTES;
            let end_sector = start + sectors * SECTOR_BYTES;
            if offset < 2 || end_sector > bytes.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Anvil chunk sector out of bounds",
                ));
            }
            let length = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
            if length == 0 || start + 4 + length > end_sector {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Anvil chunk length",
                ));
            }
            let payload = bytes[start + 4..start + 4 + length].to_vec();
            region.chunks[slot] = Some((payload, ts));
        }
        Ok(region)
    }

    fn encode(&self) -> Vec<u8> {
        let mut header = vec![0u8; HEADER_BYTES];
        let mut body = Vec::new();
        let mut next_sector: u32 = 2;

        for (slot, entry) in self.chunks.iter().enumerate() {
            let Some((payload, ts)) = entry else {
                continue;
            };
            let total_bytes = 4 + payload.len();
            let sectors = total_bytes.div_ceil(SECTOR_BYTES);
            let sectors_u8 = u8::try_from(sectors).expect("chunk fits within 255 sectors");
            let loc = (next_sector << 8) | u32::from(sectors_u8);
            header[slot * 4..slot * 4 + 4].copy_from_slice(&loc.to_be_bytes());
            header[SECTOR_BYTES + slot * 4..SECTOR_BYTES + slot * 4 + 4]
                .copy_from_slice(&ts.to_be_bytes());

            body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            body.extend_from_slice(payload);
            let padded_len = sectors * SECTOR_BYTES;
            body.resize(body.len() + (padded_len - total_bytes), 0);
            next_sector += u32::from(sectors_u8);
        }

        let mut out = Vec::with_capacity(header.len() + body.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&body);
        out
    }
}

fn state_to_nbt(state_id: i32) -> Tag {
    let reg = registry();
    let mut entry = Compound::new();
    if let Some((_, block_def, state_def)) = reg.state_by_id(state_id) {
        entry.insert("Name".into(), Tag::String(block_def.name.clone()));
        if !state_def.properties.is_empty() {
            let mut props = Compound::new();
            for (k, v) in &state_def.properties {
                props.insert(k.clone(), Tag::String(v.clone()));
            }
            entry.insert("Properties".into(), Tag::Compound(props));
        }
    } else {
        entry.insert("Name".into(), Tag::String("minecraft:air".into()));
    }
    Tag::Compound(entry)
}

fn pack_long_array(indices: &[u64], bits: usize) -> Vec<i64> {
    let per_long = 64 / bits;
    let mut longs = Vec::with_capacity(indices.len().div_ceil(per_long));
    for chunk in indices.chunks(per_long) {
        let mut packed = 0u64;
        for (i, &entry) in chunk.iter().enumerate() {
            packed |= entry << (i * bits);
        }
        longs.push(packed as i64);
    }
    longs
}

fn encode_chunk_anvil(cx: i32, cz: i32, chunk: &Chunk) -> io::Result<Vec<u8>> {
    let mut sections = Vec::with_capacity(SECTION_COUNT);

    for (sec_idx, section) in chunk.sections().iter().enumerate() {
        let mut block_states = Compound::new();
        match section.blocks() {
            BlockStorage::Single(state) => {
                block_states.insert("palette".into(), Tag::List(vec![state_to_nbt(*state)]));
            }
            BlockStorage::Dense(states) => {
                let mut palette_ids = Vec::new();
                let mut indices = [0u64; 4096];
                for (i, &state) in states.iter().enumerate() {
                    indices[i] = palette_ids
                        .iter()
                        .position(|&s| s == state)
                        .unwrap_or_else(|| {
                            palette_ids.push(state);
                            palette_ids.len() - 1
                        }) as u64;
                }
                let palette_tags: Vec<Tag> = palette_ids.into_iter().map(state_to_nbt).collect();
                let palette_len = palette_tags.len();
                block_states.insert("palette".into(), Tag::List(palette_tags));
                if palette_len > 1 {
                    let bits = ((palette_len - 1).ilog2() as usize + 1).max(4);
                    block_states.insert(
                        "data".into(),
                        Tag::LongArray(pack_long_array(&indices, bits)),
                    );
                }
            }
        }

        let mut biomes = Compound::new();
        match section.biomes() {
            BiomeStorage::Single(id) => {
                let name = biome_name(*id).unwrap_or("minecraft:plains").to_string();
                biomes.insert("palette".into(), Tag::List(vec![Tag::String(name)]));
            }
            BiomeStorage::Dense(cells) => {
                let mut palette_ids = Vec::new();
                let mut indices = [0u64; 64];
                for (i, &id) in cells.iter().enumerate() {
                    indices[i] = palette_ids
                        .iter()
                        .position(|&b| b == id)
                        .unwrap_or_else(|| {
                            palette_ids.push(id);
                            palette_ids.len() - 1
                        }) as u64;
                }
                let palette_tags: Vec<Tag> = palette_ids
                    .into_iter()
                    .map(|id| Tag::String(biome_name(id).unwrap_or("minecraft:plains").to_string()))
                    .collect();
                let palette_len = palette_tags.len();
                biomes.insert("palette".into(), Tag::List(palette_tags));
                if palette_len > 1 {
                    let bits = ((palette_len - 1).ilog2() as usize + 1).max(1);
                    biomes.insert(
                        "data".into(),
                        Tag::LongArray(pack_long_array(&indices, bits)),
                    );
                }
            }
        }

        let mut sec_map = Compound::new();
        sec_map.insert(
            "Y".into(),
            Tag::Byte((LOWEST_SECTION_Y + sec_idx as i32) as i8),
        );
        sec_map.insert("block_states".into(), Tag::Compound(block_states));
        sec_map.insert("biomes".into(), Tag::Compound(biomes));
        sections.push(Tag::Compound(sec_map));
    }

    let mut be_list = Vec::with_capacity(chunk.block_entities().len());
    for (&[x, y, z], entity) in chunk.block_entities() {
        let mut be_map = Compound::new();
        be_map.insert("x".into(), Tag::Int(x));
        be_map.insert("y".into(), Tag::Int(y));
        be_map.insert("z".into(), Tag::Int(z));
        match entity {
            BlockEntity::Chest { items } => {
                be_map.insert("id".into(), Tag::String("minecraft:chest".into()));
                be_map.insert(
                    "Items".into(),
                    Tag::List(encode_container_items(items.as_slice())),
                );
            }
            BlockEntity::Furnace(furnace) => {
                be_map.insert("id".into(), Tag::String("minecraft:furnace".into()));
                be_map.insert("BurnTime".into(), Tag::Short(furnace.burn_time as i16));
                be_map.insert("CookTime".into(), Tag::Short(furnace.cook_time as i16));
                be_map.insert(
                    "CookTimeTotal".into(),
                    Tag::Short(furnace.cook_total as i16),
                );
                be_map.insert(
                    "Items".into(),
                    Tag::List(encode_container_items(&furnace.slots())),
                );
            }
        }
        be_list.push(Tag::Compound(be_map));
    }

    let top_heights: Vec<u64> = chunk
        .heightmap(|s| s != crate::game::AIR_STATE)
        .into_iter()
        .map(u64::from)
        .collect();
    let packed_heights = pack_long_array(&top_heights, 9);
    let mut heightmaps = Compound::new();
    heightmaps.insert(
        "MOTION_BLOCKING".into(),
        Tag::LongArray(packed_heights.clone()),
    );
    heightmaps.insert("WORLD_SURFACE".into(), Tag::LongArray(packed_heights));

    let mut root = Compound::new();
    root.insert("DataVersion".into(), Tag::Int(DATA_VERSION_1_21_1));
    root.insert("xPos".into(), Tag::Int(cx));
    root.insert("zPos".into(), Tag::Int(cz));
    root.insert("yPos".into(), Tag::Int(LOWEST_SECTION_Y));
    root.insert("LastUpdate".into(), Tag::Long(0));
    root.insert("InhabitedTime".into(), Tag::Long(0));
    root.insert("Status".into(), Tag::String("minecraft:full".into()));
    root.insert("isLightOn".into(), Tag::Byte(1));
    root.insert("Heightmaps".into(), Tag::Compound(heightmaps));
    root.insert("sections".into(), Tag::List(sections));
    if !be_list.is_empty() {
        root.insert("block_entities".into(), Tag::List(be_list));
    }

    let raw_nbt = write_named_nbt("", &root)?;
    let mut encoder = ZlibEncoder::new(
        Vec::with_capacity(raw_nbt.len() / 2),
        Compression::default(),
    );
    encoder.write_all(&raw_nbt)?;
    let compressed = encoder.finish()?;

    let mut out = Vec::with_capacity(1 + compressed.len());
    out.push(2); // Compression type 2 = Zlib.
    out.extend_from_slice(&compressed);
    Ok(out)
}

fn encode_container_items(items: &[Option<ItemStack>]) -> Vec<Tag> {
    let mut items_nbt = Vec::new();
    for (slot, stack) in items.iter().enumerate() {
        let Some(stack) = stack else {
            continue;
        };
        let mut item_map = Compound::new();
        item_map.insert("Slot".into(), Tag::Byte(slot as i8));
        item_map.insert("id".into(), Tag::String(stack.item.name().into()));
        item_map.insert("count".into(), Tag::Int(i32::from(stack.count)));
        if stack.damage > 0 {
            let mut components = Compound::new();
            components.insert("minecraft:damage".into(), Tag::Int(stack.damage as i32));
            item_map.insert("components".into(), Tag::Compound(components));
        }
        items_nbt.push(Tag::Compound(item_map));
    }
    items_nbt
}

fn decode_container_items(raw_items: &[Tag], out: &mut [Option<ItemStack>]) {
    for item_tag in raw_items {
        let Some(item_map) = item_tag.as_compound() else {
            continue;
        };
        let Some(slot) = item_map
            .get("Slot")
            .and_then(Tag::as_byte)
            .and_then(|s| usize::try_from(s).ok())
            .filter(|&s| s < out.len())
        else {
            continue;
        };
        let Some(id_str) = item_map.get("id").and_then(Tag::as_str) else {
            continue;
        };
        let Some(item) = Item::by_name(id_str) else {
            continue;
        };
        let count = item_map
            .get("count")
            .and_then(Tag::as_int)
            .or_else(|| item_map.get("Count").and_then(Tag::as_byte).map(i32::from))
            .unwrap_or(1)
            .clamp(1, i32::from(item.definition().max_stack)) as u8;
        let damage = item_map
            .get("components")
            .and_then(Tag::as_compound)
            .and_then(|c| c.get("minecraft:damage"))
            .and_then(Tag::as_int)
            .unwrap_or(0)
            .max(0) as u32;
        out[slot] = ItemStack::with_damage(item, count, damage);
    }
}

fn decode_chunk_anvil(payload: &[u8]) -> io::Result<Chunk> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty Anvil chunk payload",
        ));
    }
    let compression = payload[0];
    let body = &payload[1..];
    let nbt_bytes = match compression {
        1 => {
            let mut dec = GzDecoder::new(body);
            let mut buf = Vec::new();
            dec.read_to_end(&mut buf)?;
            buf
        }
        2 => {
            let mut dec = ZlibDecoder::new(body);
            let mut buf = Vec::new();
            dec.read_to_end(&mut buf)?;
            buf
        }
        3 => body.to_vec(),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported Anvil compression type {other}"),
            ));
        }
    };
    let (_, root) = read_named_nbt(&nbt_bytes)?;
    Ok(decode_chunk_sections(&root))
}

fn decode_biomes(sec_map: &Compound) -> BiomeStorage {
    let Some(biomes_map) = sec_map.get("biomes").and_then(Tag::as_compound) else {
        return BiomeStorage::Single(PLAINS_BIOME_ID);
    };
    let Some(palette_tags) = biomes_map.get("palette").and_then(Tag::as_list) else {
        return BiomeStorage::Single(PLAINS_BIOME_ID);
    };
    if palette_tags.is_empty() {
        return BiomeStorage::Single(PLAINS_BIOME_ID);
    }
    let palette: Vec<i32> = palette_tags
        .iter()
        .map(|t| t.as_str().and_then(biome_id).unwrap_or(PLAINS_BIOME_ID))
        .collect();
    if palette.len() == 1 {
        return BiomeStorage::Single(palette[0]);
    }
    let Some(longs) = biomes_map.get("data").and_then(Tag::as_long_array) else {
        return BiomeStorage::Single(palette[0]);
    };
    let bits = ((palette.len() - 1).ilog2() as usize + 1).max(1);
    let per_long = 64 / bits;
    let mask = (1u64 << bits) - 1;
    let mut cells = Box::new([PLAINS_BIOME_ID; 64]);
    for (i, cell) in cells.iter_mut().enumerate() {
        let raw = longs.get(i / per_long).copied().unwrap_or(0) as u64;
        let pal_idx = ((raw >> ((i % per_long) * bits)) & mask) as usize;
        *cell = palette.get(pal_idx).copied().unwrap_or(PLAINS_BIOME_ID);
    }
    BiomeStorage::Dense(cells)
}

fn decode_chunk_sections(root: &Compound) -> Chunk {
    let reg = registry();
    let mut sections: Box<[ChunkSection; SECTION_COUNT]> = Box::new(std::array::from_fn(|_| {
        ChunkSection::empty(PLAINS_BIOME_ID)
    }));
    let Some(raw_sections) = root.get("sections").and_then(Tag::as_list) else {
        return Chunk::from_sections(sections);
    };
    for sec_tag in raw_sections {
        let Some(sec_map) = sec_tag.as_compound() else {
            continue;
        };
        let Some(sec_y) = sec_map.get("Y").and_then(Tag::as_byte) else {
            continue;
        };
        let sec_idx = i32::from(sec_y) - LOWEST_SECTION_Y;
        if !(0..SECTION_COUNT as i32).contains(&sec_idx) {
            continue;
        }
        let sec_idx = sec_idx as usize;
        let biomes = decode_biomes(sec_map);

        let Some(block_states) = sec_map.get("block_states").and_then(Tag::as_compound) else {
            sections[sec_idx] = ChunkSection::from_dense(Box::new([0; 4096]), biomes);
            continue;
        };
        let Some(palette_tags) = block_states.get("palette").and_then(Tag::as_list) else {
            continue;
        };
        if palette_tags.is_empty() {
            continue;
        }
        let mut palette = Vec::with_capacity(palette_tags.len());
        for entry in palette_tags {
            let state = entry
                .as_compound()
                .and_then(|map| {
                    let name = map.get("Name")?.as_str()?;
                    let mut props = BTreeMap::new();
                    if let Some(p) = map.get("Properties").and_then(Tag::as_compound) {
                        for (k, v) in p {
                            if let Some(s) = v.as_str() {
                                props.insert(k.clone(), s.to_string());
                            }
                        }
                    }
                    reg.resolve_state(name, &props)
                        .or_else(|| reg.block_by_name(name).map(|(_, b)| b.default_state))
                })
                .unwrap_or(crate::game::AIR_STATE);
            palette.push(state);
        }

        if palette.len() == 1 {
            let mut sec = ChunkSection::filled(palette[0], PLAINS_BIOME_ID);
            if !matches!(biomes, BiomeStorage::Single(PLAINS_BIOME_ID)) {
                sec = ChunkSection::from_dense(Box::new([palette[0]; 4096]), biomes);
            }
            sections[sec_idx] = sec;
            continue;
        }

        let Some(longs) = block_states.get("data").and_then(Tag::as_long_array) else {
            continue;
        };
        let bits = ((palette.len() - 1).ilog2() as usize + 1).max(4);
        let per_long = 64 / bits;
        let mask = (1u64 << bits) - 1;
        let mut blocks = Box::new([crate::game::AIR_STATE; 4096]);
        for (i, slot) in blocks.iter_mut().enumerate() {
            let raw = longs.get(i / per_long).copied().unwrap_or(0) as u64;
            let pal_idx = ((raw >> ((i % per_long) * bits)) & mask) as usize;
            *slot = palette
                .get(pal_idx)
                .copied()
                .unwrap_or(crate::game::AIR_STATE);
        }
        sections[sec_idx] = ChunkSection::from_dense(blocks, biomes);
    }
    let mut chunk = Chunk::from_sections(sections);
    if let Some(be_list) = root.get("block_entities").and_then(Tag::as_list) {
        for be_tag in be_list {
            let Some(be_map) = be_tag.as_compound() else {
                continue;
            };
            let (Some(x), Some(y), Some(z)) = (
                be_map.get("x").and_then(Tag::as_int),
                be_map.get("y").and_then(Tag::as_int),
                be_map.get("z").and_then(Tag::as_int),
            ) else {
                continue;
            };
            match be_map.get("id").and_then(Tag::as_str) {
                Some("minecraft:chest") => {
                    let mut items = Box::new([None; CHEST_SLOTS]);
                    if let Some(raw_items) = be_map.get("Items").and_then(Tag::as_list) {
                        decode_container_items(raw_items, items.as_mut_slice());
                    }
                    chunk.set_block_entity([x, y, z], BlockEntity::Chest { items });
                }
                Some("minecraft:furnace") => {
                    let mut slots = [None; 3];
                    if let Some(raw_items) = be_map.get("Items").and_then(Tag::as_list) {
                        decode_container_items(raw_items, &mut slots);
                    }
                    let burn_time = be_map
                        .get("BurnTime")
                        .and_then(Tag::as_short)
                        .unwrap_or(0)
                        .max(0) as u16;
                    let cook_time = be_map
                        .get("CookTime")
                        .and_then(Tag::as_short)
                        .unwrap_or(0)
                        .max(0) as u16;
                    let cook_total = be_map
                        .get("CookTimeTotal")
                        .and_then(Tag::as_short)
                        .unwrap_or(200)
                        .max(0) as u16;
                    chunk.set_block_entity(
                        [x, y, z],
                        BlockEntity::Furnace(Box::new(FurnaceData {
                            input: slots[0],
                            fuel: slots[1],
                            output: slots[2],
                            burn_time,
                            burn_total: burn_time,
                            cook_time,
                            cook_total,
                        })),
                    );
                }
                _ => {}
            }
        }
    }
    chunk
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{ChunkGenerator, FlatGenerator};

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
