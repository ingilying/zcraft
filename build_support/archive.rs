//! Bounded, selective extraction of registry and tag JSON from the official bundler JAR.
//! Paths come from a fixed allowlist; ZIP entry names are never joined directly
//! onto the filesystem. No Java code is executed.
use std::{
    fs,
    io::{self, Cursor, Read, Seek},
    path::Path,
};
use zip::ZipArchive;

const MAX_INNER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_JSON_BYTES: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const REGISTRIES: [&str; 11] = [
    "worldgen/biome",
    "chat_type",
    "trim_pattern",
    "trim_material",
    "wolf_variant",
    "painting_variant",
    "dimension_type",
    "damage_type",
    "banner_pattern",
    "enchantment",
    "jukebox_song",
];

pub fn extract(jar: &Path, destination: &Path) -> io::Result<()> {
    let mut outer = ZipArchive::new(fs::File::open(jar)?).map_err(invalid)?;
    let inner = outer
        .by_name("META-INF/versions/1.21.1/server-1.21.1.jar")
        .map_err(invalid)?;
    if inner.size() > MAX_INNER_BYTES {
        return Err(invalid("inner server JAR exceeds limit"));
    }
    let mut bytes = Vec::new();
    inner.take(MAX_INNER_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INNER_BYTES {
        return Err(invalid("inner server JAR exceeds limit"));
    }
    extract_inner(Cursor::new(bytes), destination)
}

fn extract_inner(reader: impl Read + Seek, destination: &Path) -> io::Result<()> {
    let mut archive = ZipArchive::new(reader).map_err(invalid)?;
    let mut total = 0;
    let mut count = 0;
    let mut seen = std::collections::HashSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(invalid)?;
        let entry_name = entry.name().to_owned();
        let relative = if let Some((registry, file)) = registry_path(&entry_name) {
            Path::new(registry).join(file)
        } else if let Some((registry, file)) = tag_path(&entry_name) {
            Path::new("tags").join(registry).join(file)
        } else {
            continue;
        };
        if !seen.insert(relative.clone()) {
            return Err(invalid("duplicate registry archive entry"));
        }
        if !entry.is_file() || entry.is_symlink() || entry.size() > MAX_JSON_BYTES {
            return Err(invalid("invalid registry archive entry"));
        }
        let mut bytes = Vec::new();
        entry.take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes)?;
        total += bytes.len();
        count += 1;
        if bytes.len() as u64 > MAX_JSON_BYTES || total > MAX_TOTAL_BYTES || count > 2048 {
            return Err(invalid("registry archive data exceeds limit"));
        }
        let output = destination.join(relative);
        fs::create_dir_all(output.parent().unwrap())?;
        fs::write(output, bytes)?;
    }
    Ok(())
}

fn registry_path(name: &str) -> Option<(&str, &str)> {
    let path = name.strip_prefix("data/minecraft/")?;
    for registry in REGISTRIES {
        let Some(file) = path
            .strip_prefix(registry)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            continue;
        };
        let stem = file.strip_suffix(".json")?;
        if !stem.is_empty()
            && stem != "."
            && stem != ".."
            && stem.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-.".contains(&byte)
            })
        {
            return Some((registry, file));
        }
    }
    None
}
fn tag_path(name: &str) -> Option<(&str, &str)> {
    let path = name.strip_prefix("data/minecraft/tags/")?;
    for registry in crate::tags::TAG_REGISTRIES {
        let Some(file) = path
            .strip_prefix(registry)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            continue;
        };
        let stem = file.strip_suffix(".json")?;
        if stem.len() <= 256
            && stem.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-.".contains(&byte)
                    })
            })
        {
            return Some((registry, file));
        }
    }
    None
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn allowlist_never_accepts_traversal_or_unrelated_assets() {
        assert_eq!(
            registry_path("data/minecraft/worldgen/biome/plains.json"),
            Some(("worldgen/biome", "plains.json"))
        );
        for name in [
            "data/minecraft/chat_type/../escape.json",
            "data/minecraft/chat_type/..json",
            "data/minecraft/chat_type/sub/a.json",
            "data/minecraft/chat_type/a\\b.json",
            "data/minecraft/chat_type/a b.json",
            "data/minecraft/recipe/test.json",
            "../data/minecraft/chat_type/a.json",
        ] {
            assert!(registry_path(name).is_none(), "{name}");
        }
    }
    #[test]
    fn tag_allowlist_accepts_nested_names_but_not_traversal_or_server_only_registries() {
        assert_eq!(
            tag_path("data/minecraft/tags/block/mineable/axe.json"),
            Some(("block", "mineable/axe.json"))
        );
        assert_eq!(
            tag_path("data/minecraft/tags/worldgen/biome/has_structure/village_plains.json"),
            Some(("worldgen/biome", "has_structure/village_plains.json"))
        );
        for name in [
            "data/minecraft/tags/block/../escape.json",
            "data/minecraft/tags/block//escape.json",
            "data/minecraft/tags/block/a\\b.json",
            "data/minecraft/tags/block/a b.json",
            "data/minecraft/tags/worldgen/structure/village.json",
            "data/minecraft/tags/worldgen/world_preset/normal.json",
        ] {
            assert!(tag_path(name).is_none(), "{name}");
        }
    }
    #[test]
    fn only_selected_registry_files_are_written() {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in [
            ("data/minecraft/chat_type/chat.json", "{}"),
            ("data/minecraft/recipe/no.json", "{}"),
            ("data/minecraft/chat_type/../escape.json", "{}"),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        let cursor = zip.finish().unwrap();
        let dir = tempfile::tempdir().unwrap();
        extract_inner(cursor, dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("chat_type/chat.json")).unwrap(),
            "{}"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(!dir.path().join("escape.json").exists());
    }
    #[test]
    fn oversized_json_and_wrong_bundle_are_rejected() {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file(
            "data/minecraft/chat_type/chat.json",
            SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(&vec![0; MAX_JSON_BYTES as usize + 1])
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(extract_inner(zip.finish().unwrap(), dir.path()).is_err());
        let wrong = dir.path().join("wrong.jar");
        fs::write(&wrong, b"not a jar").unwrap();
        assert!(extract(&wrong, dir.path()).is_err());
    }
}
