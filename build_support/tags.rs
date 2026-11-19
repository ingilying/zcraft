//! Vanilla tag resolution and protocol-767 Update Tags encoding.
//!
//! Static registry IDs come from the checked-in official data-generator report;
//! dynamic IDs come from the exact Registry Data ordering used by this build.
//! Server-only worldgen registries are deliberately excluded, matching vanilla's
//! RegistrySynchronization.networkSafeRegistries (all static + synced dynamic).
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::Path,
};

pub const TAG_REGISTRIES: [&str; 13] = [
    "banner_pattern",
    "block",
    "cat_variant",
    "damage_type",
    "enchantment",
    "entity_type",
    "fluid",
    "game_event",
    "instrument",
    "item",
    "painting_variant",
    "point_of_interest_type",
    "worldgen/biome",
];
const MAX_FILES: usize = 1024;
const MAX_JSON_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * MAX_JSON_BYTES;
const MAX_PACKET_BYTES: usize = 2_097_150;
const MAX_DEPTH: usize = 64;
const STATIC_IDS: &str = include_str!("../assets/protocol/767/static_registry_ids.json");

type Ids = BTreeMap<String, usize>;
type Definitions = BTreeMap<String, Vec<Entry>>;

pub struct TagPacket {
    pub body: Vec<u8>,
    pub counts: Vec<(String, usize)>,
}

#[derive(Clone, Debug)]
struct Entry {
    id: String,
    tag: bool,
    required: bool,
}

pub fn generate(root: &Path, dynamic_ids: &BTreeMap<String, Vec<String>>) -> io::Result<TagPacket> {
    let mut ids = static_ids()?;
    for (registry, entries) in dynamic_ids {
        let map = entries
            .iter()
            .enumerate()
            .map(|(id, name)| (name.clone(), id))
            .collect();
        if ids.insert(registry.clone(), map).is_some() {
            return Err(invalid("static and dynamic registry names overlap"));
        }
    }
    let mut body = Vec::new();
    let mut counts = Vec::new();
    let mut total_bytes = 0;
    let mut files = 0;
    varint(TAG_REGISTRIES.len(), &mut body)?;
    for registry in TAG_REGISTRIES {
        let name = format!("minecraft:{registry}");
        let entries = ids
            .get(&name)
            .ok_or_else(|| invalid(format!("missing ID map: {name}")))?;
        let directory = root.join("tags").join(registry);
        let mut definitions = Definitions::new();
        load(
            &directory,
            "",
            0,
            &mut definitions,
            &mut files,
            &mut total_bytes,
        )?;
        if definitions.is_empty() {
            return Err(invalid(format!(
                "no vanilla tags in {}",
                directory.display()
            )));
        }
        let resolved =
            resolve(&definitions, entries).map_err(|error| invalid(format!("{name}: {error}")))?;
        string(&name, &mut body)?;
        varint(resolved.len(), &mut body)?;
        for (tag, members) in &resolved {
            string(tag, &mut body)?;
            varint(members.len(), &mut body)?;
            for &member in members {
                varint(member, &mut body)?;
            }
            if body.len() > MAX_PACKET_BYTES {
                return Err(invalid("Update Tags exceeds frame limit"));
            }
        }
        counts.push((name, resolved.len()));
    }
    Ok(TagPacket { body, counts })
}

fn static_ids() -> io::Result<BTreeMap<String, Ids>> {
    let snapshot: Value = serde_json::from_str(STATIC_IDS).map_err(invalid)?;
    if snapshot["minecraft_version"] != "1.21.1"
        || snapshot["source_jar_sha1"] != "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
    {
        return Err(invalid(
            "incorrect static registry ID snapshot version/source",
        ));
    }
    let registries = snapshot["registries"]
        .as_object()
        .ok_or_else(|| invalid("invalid ID snapshot"))?;
    let mut result = BTreeMap::new();
    for (registry, entries) in registries {
        let entries = entries
            .as_object()
            .ok_or_else(|| invalid("invalid ID map"))?;
        let mut ids = Ids::new();
        for (name, id) in entries {
            identifier(name)?;
            let id =
                id.as_u64()
                    .filter(|&id| id <= i32::MAX as u64)
                    .ok_or_else(|| invalid("invalid static registry ID"))? as usize;
            ids.insert(name.clone(), id);
        }
        let unique: BTreeSet<_> = ids.values().copied().collect();
        if unique.len() != ids.len() || !unique.iter().copied().eq(0..ids.len()) {
            return Err(invalid("static IDs must be unique and contiguous"));
        }
        result.insert(registry.clone(), ids);
    }
    Ok(result)
}

fn load(
    directory: &Path,
    prefix: &str,
    depth: usize,
    definitions: &mut Definitions,
    files: &mut usize,
    total_bytes: &mut usize,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(invalid("tag directory nesting exceeds limit"));
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("non-UTF8 tag filename"))?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(invalid("tag input must not be a symlink"));
        }
        if kind.is_dir() {
            if !valid_path(&name) || name.contains('/') {
                return Err(invalid("invalid tag directory name"));
            }
            load(
                &path,
                &format!("{prefix}{name}/"),
                depth + 1,
                definitions,
                files,
                total_bytes,
            )?;
        } else if kind.is_file() && name.ends_with(".json") {
            let stem = name.strip_suffix(".json").unwrap();
            if !valid_path(stem) {
                return Err(invalid("invalid tag filename"));
            }
            *files += 1;
            if *files > MAX_FILES {
                return Err(invalid("too many tag files"));
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take(MAX_JSON_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            *total_bytes += bytes.len();
            if bytes.len() > MAX_JSON_BYTES || *total_bytes > MAX_TOTAL_BYTES {
                return Err(invalid("tag input exceeds size limit"));
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
            let tag = format!("minecraft:{prefix}{stem}");
            let entries =
                parse(&value).map_err(|error| invalid(format!("{}: {error}", path.display())))?;
            if definitions.insert(tag, entries).is_some() {
                return Err(invalid("duplicate tag definition"));
            }
        }
    }
    Ok(())
}

fn parse(value: &Value) -> io::Result<Vec<Entry>> {
    if !value.is_object()
        || value
            .get("replace")
            .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid("invalid tag object/replace flag"));
    }
    value["values"]
        .as_array()
        .ok_or_else(|| invalid("tag values must be an array"))?
        .iter()
        .map(|value| {
            let (id, required) = if let Some(id) = value.as_str() {
                (id, true)
            } else {
                let id = value["id"]
                    .as_str()
                    .ok_or_else(|| invalid("tag entry requires id"))?;
                let required = match value.get("required") {
                    Some(value) => value
                        .as_bool()
                        .ok_or_else(|| invalid("required must be boolean"))?,
                    None => true,
                };
                (id, required)
            };
            let (tag, id) = id.strip_prefix('#').map_or((false, id), |id| (true, id));
            let id = identifier(id)?;
            Ok(Entry { id, tag, required })
        })
        .collect()
}

// Resource locations default to minecraft, as the vanilla codec does.
fn identifier(id: &str) -> io::Result<String> {
    let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id));
    if namespace.is_empty()
        || namespace.contains('/')
        || !valid_path(namespace)
        || !valid_path(path)
    {
        return Err(invalid(format!("invalid tag resource location: {id}")));
    }
    Ok(format!("{namespace}:{path}"))
}

fn valid_path(path: &str) -> bool {
    path.len() <= 256
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
                })
        })
}

fn resolve(definitions: &Definitions, ids: &Ids) -> io::Result<BTreeMap<String, Vec<usize>>> {
    let mut cache = BTreeMap::new();
    let mut active = BTreeSet::new();
    for name in definitions.keys() {
        resolve_one(name, definitions, ids, &mut cache, &mut active)?;
    }
    Ok(cache)
}

fn resolve_one(
    name: &str,
    definitions: &Definitions,
    ids: &Ids,
    cache: &mut BTreeMap<String, Vec<usize>>,
    active: &mut BTreeSet<String>,
) -> io::Result<Vec<usize>> {
    if let Some(value) = cache.get(name) {
        return Ok(value.clone());
    }
    if active.len() >= MAX_DEPTH || !active.insert(name.to_owned()) {
        return Err(invalid(format!(
            "tag cycle or excessive reference depth at {name}"
        )));
    }
    let mut members = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in &definitions[name] {
        let values = if entry.tag && definitions.contains_key(&entry.id) {
            resolve_one(&entry.id, definitions, ids, cache, active)?
        } else if !entry.tag && ids.contains_key(&entry.id) {
            vec![ids[&entry.id]]
        } else if !entry.required {
            continue;
        } else {
            return Err(invalid(format!(
                "{name} references missing {}{}",
                if entry.tag { "#" } else { "" },
                entry.id
            )));
        };
        for value in values {
            // Match vanilla's insertion-ordered set: retain source order, deduplicate.
            if seen.insert(value) {
                members.push(value);
            }
        }
    }
    active.remove(name);
    cache.insert(name.to_owned(), members.clone());
    Ok(members)
}

fn varint(value: usize, output: &mut Vec<u8>) -> io::Result<()> {
    let mut value = i32::try_from(value).map_err(|_| invalid("VarInt overflow"))? as u32;
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        output.push(byte | if value != 0 { 128 } else { 0 });
        if value == 0 {
            return Ok(());
        }
    }
}
fn string(value: &str, output: &mut Vec<u8>) -> io::Result<()> {
    varint(value.len(), output)?;
    output.extend_from_slice(value.as_bytes());
    Ok(())
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definitions(values: &[(&str, Value)]) -> Definitions {
        values
            .iter()
            .map(|(name, value)| {
                (
                    format!("minecraft:{name}"),
                    parse(&json!({"values": value})).unwrap(),
                )
            })
            .collect()
    }
    fn ids() -> Ids {
        [("minecraft:a".into(), 7), ("minecraft:b".into(), 3)].into()
    }

    #[test]
    fn nested_tags_deduplicate_preserve_order_and_ignore_missing_optional_entries() {
        let defs = definitions(&[
            ("inner", json!(["a", "b", "a"])),
            (
                "outer",
                json!(["b", "#inner", {"id":"absent", "required":false}, {"id":"#absent", "required":false}]),
            ),
        ]);
        let result = resolve(&defs, &ids()).unwrap();
        assert_eq!(result["minecraft:inner"], [7, 3]);
        assert_eq!(result["minecraft:outer"], [3, 7]);
        assert_eq!(result, resolve(&defs, &ids()).unwrap());
    }
    #[test]
    fn required_missing_references_and_cycles_are_errors() {
        for values in [
            json!(["absent"]),
            json!(["#absent"]),
            json!([{"id":"absent"}]),
        ] {
            assert!(resolve(&definitions(&[("test", values)]), &ids()).is_err());
        }
        let cycle = definitions(&[("a", json!(["#b"])), ("b", json!(["#a"]))]);
        assert!(
            resolve(&cycle, &ids())
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
        let optional_cycle = definitions(&[("a", json!([{"id":"#a", "required":false}]))]);
        assert!(resolve(&optional_cycle, &ids()).is_err());
    }
    #[test]
    fn malformed_entries_and_unsafe_identifiers_are_rejected() {
        for value in [
            json!({}),
            json!({"values":false}),
            json!({"values":[7]}),
            json!({"values":[{"id":"a","required":"false"}]}),
            json!({"values":[],"replace":0}),
        ] {
            assert!(parse(&value).is_err(), "{value}");
        }
        for id in ["", "#", "../a", "a/../b", "a//b", "a:b:c", "Upper", "a\\b"] {
            assert!(identifier(id).is_err(), "{id}");
        }
    }
    #[test]
    fn snapshot_has_official_static_registry_ids_not_block_state_ids() {
        let maps = static_ids().unwrap();
        assert_eq!(maps.len(), 8);
        assert_eq!(maps["minecraft:block"].len(), 1060);
        assert_eq!(maps["minecraft:item"].len(), 1333);
        assert_eq!(maps["minecraft:block"]["minecraft:air"], 0);
        assert_eq!(maps["minecraft:block"]["minecraft:stone"], 1);
        assert_eq!(maps["minecraft:block"]["minecraft:oak_log"], 46); // Block states start at 130, not registry ID 46.
    }
}
