//! Official Java 1.21.1 registries generated at build time.
//! Acquisition, JSON conversion and NBT encoding live in build_support/.
//! Runtime only copies pre-encoded packet bodies; no JSON parsing or downloads.
use std::io;

mod generated {
    include!(concat!(env!("OUT_DIR"), "/registries.rs"));
}
pub use generated::{
    MINECRAFT_VERSION, OVERWORLD_DIMENSION_ID, PLAINS_BIOME_ID, REGISTRY_COUNTS, REGISTRY_NAMES,
    SOURCE_JAR_SHA1, TAG_COUNTS, TAG_REGISTRY_NAMES,
};

pub fn registry_packets() -> io::Result<Vec<Vec<u8>>> {
    Ok(generated::REGISTRY_PACKETS
        .iter()
        .map(|packet| packet.to_vec())
        .collect())
}

/// Full vanilla network-safe tags, using the same dynamic registry IDs as
/// `registry_packets()` and official static registry IDs (not block state IDs).
pub fn tags_packet() -> io::Result<Vec<u8>> {
    Ok(generated::TAGS_PACKET.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const MAX_PACKET_BYTES: usize = 2_097_150;

    // Independent wire scanner. Check every entry and byte, not just JSON input.
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, len: usize) -> &'a [u8] {
            let (head, tail) = self.0.split_at(len);
            self.0 = tail;
            head
        }
        fn byte(&mut self) -> u8 {
            self.take(1)[0]
        }
        fn int(&mut self) -> usize {
            let value = i32::from_be_bytes(self.take(4).try_into().unwrap());
            assert!(value >= 0);
            value as usize
        }
        fn varint(&mut self) -> usize {
            let mut value = 0;
            for shift in (0..35).step_by(7) {
                let byte = self.byte();
                value |= usize::from(byte & 127) << shift;
                if byte & 128 == 0 {
                    return value;
                }
            }
            panic!("invalid VarInt")
        }
        fn string(&mut self) -> String {
            let len = self.varint();
            String::from_utf8(self.take(len).to_vec()).unwrap()
        }
        fn nbt_string(&mut self) -> String {
            let len = u16::from_be_bytes(self.take(2).try_into().unwrap()) as usize;
            String::from_utf8(self.take(len).to_vec()).unwrap()
        }
        fn payload(&mut self, id: u8) {
            match id {
                1 => {
                    self.take(1);
                }
                2 => {
                    self.take(2);
                }
                3 | 5 => {
                    self.take(4);
                }
                4 | 6 => {
                    self.take(8);
                }
                7 => {
                    let len = self.int();
                    self.take(len);
                }
                8 => {
                    self.nbt_string();
                }
                9 => {
                    let kind = self.byte();
                    let len = self.int();
                    for _ in 0..len {
                        self.payload(kind);
                    }
                }
                10 => loop {
                    let kind = self.byte();
                    if kind == 0 {
                        break;
                    }
                    self.nbt_string();
                    self.payload(kind);
                },
                11 | 12 => {
                    let len = self.int();
                    self.take(len * if id == 11 { 4 } else { 8 });
                }
                _ => panic!("invalid tag {id}"),
            }
        }
    }

    #[test]
    fn all_registries_encode_full_entries_and_fit_frames() {
        let expected = [
            ("minecraft:worldgen/biome", 64),
            ("minecraft:chat_type", 7),
            ("minecraft:trim_pattern", 18),
            ("minecraft:trim_material", 10),
            ("minecraft:wolf_variant", 9),
            ("minecraft:painting_variant", 50),
            ("minecraft:dimension_type", 4),
            ("minecraft:damage_type", 47),
            ("minecraft:banner_pattern", 43),
            ("minecraft:enchantment", 42),
            ("minecraft:jukebox_song", 19),
        ];
        let packets = registry_packets().unwrap();
        assert_eq!(packets, registry_packets().unwrap());
        assert_eq!(packets.len(), expected.len());
        for (packet, (name, count)) in packets.iter().zip(expected) {
            assert!(packet.len() < MAX_PACKET_BYTES);
            let mut reader = Reader(packet);
            assert_eq!(reader.string(), name);
            assert_eq!(reader.varint(), count);
            let mut names = BTreeSet::new();
            for id in 0..count {
                let key = reader.string();
                if name == "minecraft:worldgen/biome" && id == 0 {
                    assert_eq!(key, "minecraft:plains");
                }
                if name == "minecraft:dimension_type" && id == 0 {
                    assert_eq!(key, "minecraft:overworld");
                }
                assert!(names.insert(key));
                assert_eq!(reader.byte(), 1);
                assert_eq!(reader.byte(), 10);
                reader.payload(10);
            }
            assert!(reader.0.is_empty());
        }
    }

    #[test]
    fn generated_metadata_matches_world_contract() {
        assert_eq!(MINECRAFT_VERSION, "1.21.1");
        assert_eq!(SOURCE_JAR_SHA1, "59353fb40c36d304f2035d51e7d6e6baa98dc05c");
        assert_eq!(PLAINS_BIOME_ID, 0);
        assert_eq!(OVERWORLD_DIMENSION_ID, 0);
        assert_eq!(REGISTRY_NAMES.len(), 11);
        assert_eq!(REGISTRY_COUNTS.iter().sum::<usize>(), 313);
        assert_eq!(TAG_REGISTRY_NAMES.len(), 13);
        assert_eq!(TAG_COUNTS.iter().sum::<usize>(), 514);
        assert!(tags_packet().unwrap().len() > 1);
    }
}
