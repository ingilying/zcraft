//! Cache integrity checks shared by build.rs and offline unit tests.
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
};

pub const VERSION: &str = "1.21.1";
pub const SERVER_SHA1: &str = "59353fb40c36d304f2035d51e7d6e6baa98dc05c";
pub const SERVER_BYTES: u64 = 51_627_615;
pub const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

pub fn verify_jar(path: &Path) -> io::Result<()> {
    let mut file = fs::File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() != SERVER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected the official Minecraft 1.21.1 bundler JAR (51627615 bytes)",
        ));
    }
    let mut hasher = Sha1::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if format!("{:x}", hasher.finalize()) != SERVER_SHA1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "official server JAR SHA-1 mismatch",
        ));
    }
    Ok(())
}

pub fn read_source(directory: &Path, generator: &str) -> io::Result<Option<String>> {
    let source = directory.join("registries.rs");
    let manifest = directory.join("manifest.json");
    let read = || -> io::Result<Option<String>> {
        let metadata = read_bounded(&manifest, 8192)?;
        let metadata: serde_json::Value =
            serde_json::from_slice(&metadata).map_err(io::Error::other)?;
        if metadata["format"] != 1
            || metadata["minecraft"] != VERSION
            || metadata["server_sha1"] != SERVER_SHA1
            || metadata["generator_sha1"] != generator
        {
            return Ok(None);
        }
        let bytes = read_bounded(&source, MAX_SOURCE_BYTES)?;
        if metadata["source_sha1"] != digest(&bytes) {
            return Ok(None);
        }
        Ok(Some(String::from_utf8(bytes).map_err(io::Error::other)?))
    };
    match read() {
        Ok(value) => Ok(value),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData | io::ErrorKind::Other
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn publish(directory: &Path, generator: &str, source: &str) -> io::Result<()> {
    if source.len() as u64 > MAX_SOURCE_BYTES {
        return Err(io::Error::other("generated registry source is too large"));
    }
    fs::create_dir_all(directory)?;
    let metadata = serde_json::json!({"format":1,"minecraft":VERSION,"server_sha1":SERVER_SHA1,"generator_sha1":generator,"source_sha1":digest(source.as_bytes())});
    atomic_write(&directory.join("registries.rs"), source.as_bytes())?;
    atomic_write(
        &directory.join("manifest.json"),
        serde_json::to_string_pretty(&metadata)?.as_bytes(),
    )
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("output has no parent directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}
fn read_bounded(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry exceeds limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_cache_requires_matching_source_version_and_generator() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_source(dir.path(), "generator").unwrap().is_none());
        publish(dir.path(), "generator", "pub const TEST:u8=1;").unwrap();
        assert_eq!(
            read_source(dir.path(), "generator").unwrap().unwrap(),
            "pub const TEST:u8=1;"
        );
        assert!(read_source(dir.path(), "different").unwrap().is_none());
        fs::write(dir.path().join("registries.rs"), "corrupt").unwrap();
        assert!(read_source(dir.path(), "generator").unwrap().is_none());
        publish(dir.path(), "generator", "valid").unwrap();
        fs::write(dir.path().join("manifest.json"), "not JSON").unwrap();
        assert!(read_source(dir.path(), "generator").unwrap().is_none());
    }
    #[test]
    fn arbitrary_local_jars_are_not_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("server.jar");
        fs::write(&file, b"wrong version").unwrap();
        assert!(verify_jar(&file).is_err());
        let handle = fs::OpenOptions::new().write(true).open(&file).unwrap();
        handle.set_len(SERVER_BYTES).unwrap();
        assert!(verify_jar(&file).is_err());
    }
}
