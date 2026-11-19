use std::collections::{BTreeMap, BTreeSet};
use zcraft::{protocol::codec::Reader, registry_data};

#[path = "../build_support/archive.rs"]
mod archive;
#[path = "../build_support/tags.rs"]
mod tags;

type Tags = BTreeMap<String, BTreeMap<String, Vec<usize>>>;

fn count(reader: &mut Reader<'_>) -> usize {
    usize::try_from(reader.varint().unwrap()).unwrap()
}

fn decode_tags(body: &[u8]) -> Tags {
    let mut reader = Reader::new(body);
    let mut registries = BTreeMap::new();
    for _ in 0..count(&mut reader) {
        let name = reader.string(32767).unwrap();
        let mut tags = BTreeMap::new();
        for _ in 0..count(&mut reader) {
            let tag = reader.string(32767).unwrap();
            let members: Vec<_> = (0..count(&mut reader))
                .map(|_| count(&mut reader))
                .collect();
            assert_eq!(members.iter().collect::<BTreeSet<_>>().len(), members.len());
            assert!(tags.insert(tag, members).is_none());
        }
        assert!(registries.insert(name, tags).is_none());
    }
    reader.finish().unwrap();
    registries
}

// Independent NBT walker: discover IDs from the bytes actually sent, rather than
// assuming the generator's sort order or relying on its internal ID map.
fn skip_nbt(kind: u8, reader: &mut Reader<'_>) {
    match kind {
        1 => {
            reader.take(1).unwrap();
        }
        2 => {
            reader.take(2).unwrap();
        }
        3 | 5 => {
            reader.take(4).unwrap();
        }
        4 | 6 => {
            reader.take(8).unwrap();
        }
        7 | 11 | 12 => {
            let count = i32::from_be_bytes(reader.take(4).unwrap().try_into().unwrap());
            let width = match kind {
                7 => 1,
                11 => 4,
                _ => 8,
            };
            reader
                .take(usize::try_from(count).unwrap() * width)
                .unwrap();
        }
        8 => {
            let count = reader.u16().unwrap();
            reader.take(count as usize).unwrap();
        }
        9 => {
            let kind = reader.take(1).unwrap()[0];
            let count = i32::from_be_bytes(reader.take(4).unwrap().try_into().unwrap());
            assert!(count >= 0);
            for _ in 0..count {
                skip_nbt(kind, reader);
            }
        }
        10 => loop {
            let kind = reader.take(1).unwrap()[0];
            if kind == 0 {
                break;
            }
            let count = reader.u16().unwrap();
            reader.take(count as usize).unwrap();
            skip_nbt(kind, reader);
        },
        _ => panic!("invalid NBT type {kind}"),
    }
}

fn dynamic_ids() -> BTreeMap<String, Vec<String>> {
    let mut ids = BTreeMap::new();
    for packet in registry_data::registry_packets().unwrap() {
        let mut reader = Reader::new(&packet);
        let registry = reader.string(32767).unwrap();
        let mut entries = Vec::new();
        for _ in 0..count(&mut reader) {
            entries.push(reader.string(32767).unwrap());
            assert_eq!(reader.take(1).unwrap(), [1]);
            let kind = reader.take(1).unwrap()[0];
            skip_nbt(kind, &mut reader);
        }
        reader.finish().unwrap();
        ids.insert(registry, entries);
    }
    ids
}

#[test]
fn full_update_tags_is_deterministic_complete_bounded_and_uses_valid_registry_ids() {
    let packet = registry_data::tags_packet().unwrap();
    assert_eq!(packet, registry_data::tags_packet().unwrap());
    assert!(packet.len() < 2_097_150);
    let wire = decode_tags(&packet);
    let expected = [
        ("banner_pattern", 9),
        ("block", 184),
        ("cat_variant", 2),
        ("damage_type", 32),
        ("enchantment", 22),
        ("entity_type", 34),
        ("fluid", 2),
        ("game_event", 5),
        ("instrument", 3),
        ("item", 147),
        ("painting_variant", 1),
        ("point_of_interest_type", 3),
        ("worldgen/biome", 70),
    ];
    assert_eq!(wire.len(), expected.len());
    assert_eq!(registry_data::TAG_COUNTS.iter().sum::<usize>(), 514);
    let static_maps: serde_json::Value = serde_json::from_str(include_str!(
        "../assets/protocol/767/static_registry_ids.json"
    ))
    .unwrap();
    let dynamic = dynamic_ids();
    for (name, expected_tags) in expected {
        let registry = format!("minecraft:{name}");
        let tags = &wire[&registry];
        assert_eq!(tags.len(), expected_tags, "{registry}");
        let limit = dynamic.get(&registry).map(Vec::len).unwrap_or_else(|| {
            static_maps["registries"][&registry]
                .as_object()
                .unwrap()
                .len()
        });
        for (tag, members) in tags {
            assert!(tag.starts_with("minecraft:"));
            for &id in members {
                assert!(id < limit, "{registry} {tag} invalid ID {id}");
            }
        }
    }
    for server_only in [
        "worldgen/structure",
        "worldgen/world_preset",
        "worldgen/flat_level_generator_preset",
    ] {
        assert!(!wire.contains_key(&format!("minecraft:{server_only}")));
    }
    assert_eq!(wire["minecraft:fluid"]["minecraft:water"], [2, 1]);
    assert_eq!(wire["minecraft:fluid"]["minecraft:lava"], [4, 3]);
    // These registry IDs are from Mojang's registry report, not block-state IDs.
    assert!(wire["minecraft:block"]["minecraft:logs"].contains(&46));
    assert!(wire["minecraft:block"]["minecraft:logs"].contains(&47));
    let biome_ids = &dynamic["minecraft:worldgen/biome"];
    let plains = biome_ids
        .iter()
        .position(|name| name == "minecraft:plains")
        .unwrap();
    let ocean = biome_ids
        .iter()
        .position(|name| name == "minecraft:ocean")
        .unwrap();
    assert_eq!(plains, 0);
    assert!(wire["minecraft:worldgen/biome"]["minecraft:is_overworld"].contains(&plains));
    assert!(wire["minecraft:worldgen/biome"]["minecraft:is_ocean"].contains(&ocean));
    assert!(!wire["minecraft:worldgen/biome"]["minecraft:is_ocean"].contains(&plains));
}

#[test]
fn fresh_official_jar_tags_resolve_to_the_exact_runtime_bytes_when_source_is_available() {
    let Some(jar) = std::env::var_os("ZCRAFT_TEST_SERVER_JAR") else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    archive::extract(std::path::Path::new(&jar), directory.path()).unwrap();
    let ids = dynamic_ids();
    let first = tags::generate(directory.path(), &ids).unwrap();
    let second = tags::generate(directory.path(), &ids).unwrap();
    assert_eq!(first.body, second.body);
    assert_eq!(first.body, registry_data::tags_packet().unwrap());
    assert_eq!(
        first.counts.iter().map(|(_, count)| count).sum::<usize>(),
        514
    );
    // Missing required elements/tags must fail, not silently create partial tags.
    std::fs::write(
        directory.path().join("tags/block/logs.json"),
        br#"{"values":["minecraft:missing_block"]}"#,
    )
    .unwrap();
    assert!(tags::generate(directory.path(), &ids).is_err());
}
