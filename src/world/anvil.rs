//! Vanilla Minecraft Java 1.21.1 (DataVersion 3955) Anvil persistence:
//! - `level.dat`: GZip-compressed NBT world metadata & flat generator settings
//! - `region/r.<rx>.<rz>.mca`: 32x32 chunk region files (Zlib-compressed NBT)
//! - `playerdata/<uuid>.dat`: GZip-compressed NBT per-player state & inventory
use super::{
    MIN_Y, SECTION_COUNT,
    clock::PersistedClock,
    edits::Edits,
    generator::{FlatGenerator, FlatGeneratorConfig, FlatLayerConfig},
    nbt::{
        Compound, Tag, read_gzip_nbt, read_named_nbt, write_bytes_atomic, write_gzip_nbt_atomic,
        write_named_nbt,
    },
};
use crate::{
    config::GameMode,
    game::{Inventory, ItemStack, Player, SLOT_COUNT, item_by_id, item_by_name, registry},
};
use flate2::{
    Compression,
    read::{GzDecoder, ZlibDecoder},
    write::ZlibEncoder,
};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
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
    pub flat: Option<FlatGeneratorConfig>,
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

        let flat = parse_flat_settings(data);
        Ok(Some(LoadedLevel { clock, flat, spawn }))
    }

    pub fn save_level(
        &self,
        clock: PersistedClock,
        generator: &FlatGenerator,
        spawn: [f64; 3],
    ) -> io::Result<()> {
        let level_name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("world")
            .to_string();

        let mut layers = Vec::with_capacity(generator.config().layers.len());
        for layer in &generator.config().layers {
            let mut lc = Compound::new();
            lc.insert("block".into(), Tag::String(layer.block.clone()));
            lc.insert("height".into(), Tag::Int(i32::from(layer.height)));
            layers.push(Tag::Compound(lc));
        }

        let mut settings = Compound::new();
        settings.insert(
            "biome".into(),
            Tag::String(generator.config().biome.clone()),
        );
        settings.insert("layers".into(), Tag::List(layers));
        settings.insert("zcraft_base_y".into(), Tag::Int(generator.config().base_y));

        let mut gen_compound = Compound::new();
        gen_compound.insert("type".into(), Tag::String("minecraft:flat".into()));
        gen_compound.insert("settings".into(), Tag::Compound(settings));

        let mut overworld = Compound::new();
        overworld.insert("type".into(), Tag::String("minecraft:overworld".into()));
        overworld.insert("generator".into(), Tag::Compound(gen_compound));

        let mut dimensions = Compound::new();
        dimensions.insert("minecraft:overworld".into(), Tag::Compound(overworld));

        let mut world_gen = Compound::new();
        world_gen.insert("bonus_chest".into(), Tag::Byte(0));
        world_gen.insert("generate_features".into(), Tag::Byte(0));
        world_gen.insert("seed".into(), Tag::Long(0));
        world_gen.insert("dimensions".into(), Tag::Compound(dimensions));

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);

        let mut data = Compound::new();
        data.insert("DataVersion".into(), Tag::Int(DATA_VERSION_1_21_1));
        data.insert("version".into(), Tag::Int(ANVIL_VERSION));
        data.insert("LevelName".into(), Tag::String(level_name));
        data.insert("Time".into(), Tag::Long(clock.world_age));
        data.insert("DayTime".into(), Tag::Long(clock.day_time));
        data.insert("raining".into(), Tag::Byte(i8::from(clock.raining)));
        data.insert("rainTime".into(), Tag::Int(clock.rain_time as i32));
        data.insert("thundering".into(), Tag::Byte(i8::from(clock.thundering)));
        data.insert("thunderTime".into(), Tag::Int(clock.thunder_time as i32));
        data.insert("SpawnX".into(), Tag::Int(spawn[0].floor() as i32));
        data.insert("SpawnY".into(), Tag::Int(spawn[1].floor() as i32));
        data.insert("SpawnZ".into(), Tag::Int(spawn[2].floor() as i32));
        data.insert("LastPlayed".into(), Tag::Long(now_ms));
        data.insert("initialized".into(), Tag::Byte(1));
        data.insert("WorldGenSettings".into(), Tag::Compound(world_gen));

        let mut root = Compound::new();
        root.insert("Data".into(), Tag::Compound(data));
        write_gzip_nbt_atomic(&self.level_dat_path(), "", &root)
    }

    /// Load all persisted chunk edits across all `region/r.<rx>.<rz>.mca` files.
    pub fn load_all_chunks(
        &self,
        generator: &FlatGenerator,
    ) -> io::Result<HashMap<[i32; 2], Edits>> {
        let dir = self.region_dir();
        if !dir.exists() {
            return Ok(HashMap::new());
        }
        let mut loaded = HashMap::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("r.") || !name.ends_with(".mca") {
                continue;
            }
            let raw = fs::read(&path)?;
            for (chunk_coords, edits) in decode_region_chunks(&raw, generator)? {
                if !edits.is_empty() {
                    loaded.insert(chunk_coords, edits);
                }
            }
        }
        Ok(loaded)
    }

    /// Save a batch of dirty chunks grouped by region file (`r.<rx>.<rz>.mca`).
    /// Chunks with `None` edits are still updated (re-written as flat or cleared).
    pub fn save_chunks(
        &self,
        chunks: &[([i32; 2], Option<Edits>)],
        generator: &FlatGenerator,
    ) -> io::Result<usize> {
        if chunks.is_empty() {
            return Ok(0);
        }
        type RegionUpdate<'a> = ([i32; 2], Option<&'a Edits>);
        let mut by_region: BTreeMap<[i32; 2], Vec<RegionUpdate<'_>>> = BTreeMap::new();
        for (coords, edits) in chunks {
            let region = [coords[0].div_euclid(32), coords[1].div_euclid(32)];
            by_region
                .entry(region)
                .or_default()
                .push((*coords, edits.as_ref()));
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
            for ([cx, cz], edits) in updates {
                let slot = region_slot(cx, cz);
                let payload = encode_chunk_anvil(cx, cz, edits, generator)?;
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
            Some(1) => GameMode::Creative,
            _ => GameMode::Spectator,
        };
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
                let Some(def) = item_by_name(id_str) else {
                    continue;
                };
                let count = item_map
                    .get("count")
                    .and_then(Tag::as_int)
                    .or_else(|| item_map.get("Count").and_then(Tag::as_byte).map(i32::from))
                    .unwrap_or(1)
                    .clamp(1, i32::from(def.max_stack)) as u8;
                let damage = item_map
                    .get("components")
                    .and_then(Tag::as_compound)
                    .and_then(|c| c.get("minecraft:damage"))
                    .and_then(Tag::as_int)
                    .unwrap_or(0)
                    .max(0) as u32;
                slots[internal_slot] = ItemStack::with_damage(def.id, count, damage);
            }
        }
        let inventory = Inventory::from_persisted(slots, selected);
        Ok(Some(SavedPlayer {
            position,
            yaw,
            pitch,
            on_ground,
            game_mode,
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
            let Some(def) = item_by_id(stack.item) else {
                continue;
            };
            let mut item_map = Compound::new();
            item_map.insert("Slot".into(), Tag::Byte(vanilla_slot));
            item_map.insert("id".into(), Tag::String(def.name.clone()));
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
        -106 => Some(45),
        _ => None,
    }
}

fn internal_to_vanilla_slot(slot: usize) -> Option<i8> {
    match slot {
        36..=44 => Some((slot - 36) as i8),
        9..=35 => Some(slot as i8),
        45 => Some(-106),
        _ => None,
    }
}

fn parse_flat_settings(data: &Compound) -> Option<FlatGeneratorConfig> {
    let generator = data
        .get("WorldGenSettings")?
        .as_compound()?
        .get("dimensions")?
        .as_compound()?
        .get("minecraft:overworld")?
        .as_compound()?
        .get("generator")?
        .as_compound()?;
    if generator.get("type")?.as_str()? != "minecraft:flat" {
        return None;
    }
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
    Some(cfg)
}

fn region_slot(cx: i32, cz: i32) -> usize {
    cx.rem_euclid(32) as usize + cz.rem_euclid(32) as usize * 32
}

type RegionSlot = Option<(Vec<u8>, u32)>;

struct RegionFile {
    /// Each slot holds `(raw_chunk_payload_with_compression_byte, timestamp_secs)`.
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

fn encode_chunk_anvil(
    cx: i32,
    cz: i32,
    edits: Option<&Edits>,
    generator: &FlatGenerator,
) -> io::Result<Vec<u8>> {
    let reg = registry();
    let mut sections = Vec::with_capacity(SECTION_COUNT);

    for section in 0..SECTION_COUNT {
        let offset = (section * 4096) as u32;
        let mut states = [0i32; 4096];
        for (ly, layer) in states.as_chunks_mut::<256>().0.iter_mut().enumerate() {
            layer.fill(generator.base_state(MIN_Y + (section * 16 + ly) as i32));
        }
        if let Some(edits) = edits {
            for (&idx, &state) in edits.range(offset..offset + 4096) {
                states[(idx - offset) as usize] = state;
            }
        }

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

        let mut palette_tags = Vec::with_capacity(palette_ids.len());
        for state_id in palette_ids {
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
            palette_tags.push(Tag::Compound(entry));
        }

        let mut block_states = Compound::new();
        let palette_len = palette_tags.len();
        block_states.insert("palette".into(), Tag::List(palette_tags));
        if palette_len > 1 {
            let bits = ((palette_len - 1).ilog2() as usize + 1).max(4);
            let per_long = 64 / bits;
            let mut longs = Vec::with_capacity(4096usize.div_ceil(per_long));
            for chunk in indices.chunks(per_long) {
                let mut packed = 0u64;
                for (i, &entry) in chunk.iter().enumerate() {
                    packed |= entry << (i * bits);
                }
                longs.push(packed as i64);
            }
            block_states.insert("data".into(), Tag::LongArray(longs));
        }

        let mut biomes = Compound::new();
        biomes.insert(
            "palette".into(),
            Tag::List(vec![Tag::String(generator.config().biome.clone())]),
        );

        let mut sec_map = Compound::new();
        sec_map.insert(
            "Y".into(),
            Tag::Byte((LOWEST_SECTION_Y + section as i32) as i8),
        );
        sec_map.insert("block_states".into(), Tag::Compound(block_states));
        sec_map.insert("biomes".into(), Tag::Compound(biomes));
        sections.push(Tag::Compound(sec_map));
    }

    let mut root = Compound::new();
    root.insert("DataVersion".into(), Tag::Int(DATA_VERSION_1_21_1));
    root.insert("xPos".into(), Tag::Int(cx));
    root.insert("zPos".into(), Tag::Int(cz));
    root.insert("yPos".into(), Tag::Int(LOWEST_SECTION_Y));
    root.insert("Status".into(), Tag::String("minecraft:full".into()));
    root.insert("sections".into(), Tag::List(sections));

    let raw_nbt = write_named_nbt("", &root)?;
    let mut encoder = ZlibEncoder::new(
        Vec::with_capacity(raw_nbt.len() / 2),
        Compression::default(),
    );
    encoder.write_all(&raw_nbt)?;
    let compressed = encoder.finish()?;

    let mut out = Vec::with_capacity(1 + compressed.len());
    out.push(2); // Compression type 2 = Zlib (vanilla Anvil default).
    out.extend_from_slice(&compressed);
    Ok(out)
}

fn decode_region_chunks(
    bytes: &[u8],
    generator: &FlatGenerator,
) -> io::Result<Vec<([i32; 2], Edits)>> {
    let region = RegionFile::parse(bytes)?;
    let mut out = Vec::new();
    for entry in region.chunks.iter().flatten() {
        let (payload, _) = entry;
        if payload.is_empty() {
            continue;
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
        let Some(cx) = root.get("xPos").and_then(Tag::as_int) else {
            continue;
        };
        let Some(cz) = root.get("zPos").and_then(Tag::as_int) else {
            continue;
        };
        let edits = decode_chunk_sections(&root, generator);
        out.push(([cx, cz], edits));
    }
    Ok(out)
}

fn decode_chunk_sections(root: &Compound, generator: &FlatGenerator) -> Edits {
    let reg = registry();
    let mut edits = Edits::new();
    let Some(sections) = root.get("sections").and_then(Tag::as_list) else {
        return edits;
    };
    for sec_tag in sections {
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
        let Some(block_states) = sec_map.get("block_states").and_then(Tag::as_compound) else {
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

        let offset = (sec_idx * 4096) as u32;
        if palette.len() == 1 {
            let state = palette[0];
            for ly in 0..16 {
                let world_y = MIN_Y + (sec_idx * 16 + ly) as i32;
                if state != generator.base_state(world_y) {
                    let layer_start = offset + (ly * 256) as u32;
                    for col in 0..256u32 {
                        edits.insert(layer_start + col, state);
                    }
                }
            }
            continue;
        }

        let Some(longs) = block_states.get("data").and_then(Tag::as_long_array) else {
            continue;
        };
        let bits = ((palette.len() - 1).ilog2() as usize + 1).max(4);
        let per_long = 64 / bits;
        let mask = (1u64 << bits) - 1;
        for i in 0..4096usize {
            let long_idx = i / per_long;
            let bit_off = (i % per_long) * bits;
            let raw = longs.get(long_idx).copied().unwrap_or(0) as u64;
            let pal_idx = ((raw >> bit_off) & mask) as usize;
            let state = palette
                .get(pal_idx)
                .copied()
                .unwrap_or(crate::game::AIR_STATE);
            let ly = i / 256;
            let world_y = MIN_Y + (sec_idx * 16 + ly) as i32;
            if state != generator.base_state(world_y) {
                edits.insert(offset + i as u32, state);
            }
        }
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::BlockPos;

    #[test]
    fn level_dat_region_mca_and_playerdata_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = AnvilStorage::new(dir.path());
        let generator = FlatGenerator::default();

        // 1. level.dat
        let clock = PersistedClock {
            world_age: 12_345,
            day_time: 18_000,
            raining: true,
            rain_time: 5_000,
            thundering: false,
            thunder_time: 9_000,
        };
        storage
            .save_level(clock, &generator, [8.5, 65.0, 8.5])
            .unwrap();
        let loaded_level = storage.load_level().unwrap().unwrap();
        assert_eq!(loaded_level.clock, clock);
        assert_eq!(loaded_level.flat.as_ref(), Some(generator.config()));

        // 2. region/r.-1.0.mca and r.0.0.mca with block state properties (oak_stairs / torch / stone)
        let mut c0 = Edits::new();
        let pos_stone = BlockPos([3, 64, 5]);
        let pos_bedrock_broken = BlockPos([8, 60, 8]);
        let mut props = BTreeMap::new();
        props.insert("facing".into(), "east".into());
        let stairs_state = registry()
            .resolve_state("minecraft:oak_stairs", &props)
            .unwrap();
        c0.insert(pos_stone.index(), stairs_state);
        c0.insert(pos_bedrock_broken.index(), crate::game::AIR_STATE);

        let mut c_neg = Edits::new();
        let pos_neg = BlockPos([-2, 65, 1]);
        c_neg.insert(pos_neg.index(), crate::game::GLASS_STATE);

        let saved = storage
            .save_chunks(
                &[([0, 0], Some(c0.clone())), ([-1, 0], Some(c_neg.clone()))],
                &generator,
            )
            .unwrap();
        assert_eq!(saved, 2);
        assert!(dir.path().join("region/r.0.0.mca").exists());
        assert!(dir.path().join("region/r.-1.0.mca").exists());

        let reloaded = storage.load_all_chunks(&generator).unwrap();
        assert_eq!(reloaded.get(&[0, 0]), Some(&c0));
        assert_eq!(reloaded.get(&[-1, 0]), Some(&c_neg));

        // 3. playerdata/<uuid>.dat
        let uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let mut player = Player::at_spawn(1, uuid, "Steve".into(), GameMode::Creative);
        player.position = [12.25, 70.5, -4.75];
        player.yaw = 90.0;
        player.pitch = -15.5;
        player.on_ground = true;
        let mut inv = Inventory::default();
        inv.select(3);
        let pickaxe_id = item_by_name("iron_pickaxe").unwrap().id;
        inv.set_creative(45, ItemStack::with_damage(pickaxe_id, 1, 17));

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
            ItemStack::with_damage(pickaxe_id, 1, 17)
        );
    }
}
