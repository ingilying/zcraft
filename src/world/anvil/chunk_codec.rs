//! Chunk <-> Anvil 1.21.1 NBT serialization, palette bit-packing, and block entities.
use super::DATA_VERSION_1_21_1;
use crate::{
    game::{Item, ItemStack, registry},
    registry_data::{biome_id, biome_name},
    world::{
        BiomeStorage, BlockEntity, BlockStorage, CHEST_SLOTS, Chunk, ChunkSection, FurnaceData,
        MIN_Y, PLAINS_BIOME_ID, SECTION_COUNT,
        nbt::{Compound, Tag, read_named_nbt, write_named_nbt},
    },
};
use flate2::{
    Compression,
    read::{GzDecoder, ZlibDecoder},
    write::ZlibEncoder,
};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
};

const LOWEST_SECTION_Y: i32 = MIN_Y / 16; // -4

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

pub(super) fn encode_chunk_anvil(cx: i32, cz: i32, chunk: &Chunk) -> io::Result<Vec<u8>> {
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

pub(super) fn decode_chunk_anvil(payload: &[u8]) -> io::Result<Chunk> {
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
