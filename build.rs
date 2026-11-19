//! Build missing protocol data from a pinned official JAR, never a running server.
#[path = "build_support/archive.rs"]
mod archive;
#[path = "build_support/cache.rs"]
mod cache;
#[path = "build_support/registry.rs"]
mod registry;
#[path = "build_support/tags.rs"]
mod tags;

use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const SERVER_URL: &str =
    "https://piston-data.mojang.com/v1/objects/59353fb40c36d304f2035d51e7d6e6baa98dc05c/server.jar";

fn main() {
    for file in [
        "build.rs",
        "build_support/registry.rs",
        "build_support/archive.rs",
        "build_support/cache.rs",
        "build_support/tags.rs",
        "assets/protocol/767/static_registry_ids.json",
    ] {
        println!("cargo:rerun-if-changed={file}");
    }
    for name in [
        "ZCRAFT_SERVER_JAR",
        "ZCRAFT_DATA_CACHE",
        "ZCRAFT_DATA_OFFLINE",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if let Err(error) = build() {
        panic!(
            "cannot prepare Minecraft {} registries: {error}. Use ZCRAFT_SERVER_JAR=/path/to/official-server.jar for a local source, or reuse ZCRAFT_DATA_CACHE from a successful build",
            cache::VERSION
        );
    }
}

fn build() -> io::Result<()> {
    let root = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| io::Error::other("missing manifest directory"))?,
    );
    let cache_root = env::var_os("ZCRAFT_DATA_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/zcraft-data"));
    let offline = match env::var("ZCRAFT_DATA_OFFLINE").as_deref() {
        Ok("1" | "true") => true,
        Ok("0" | "false") | Err(env::VarError::NotPresent) => false,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZCRAFT_DATA_OFFLINE must be true/false or 1/0",
            ));
        }
    };
    // Any generator/acquisition change invalidates generated constants, while
    // the pinned official JAR can still be reused without another download.
    let fingerprint = cache::digest(
        &[
            include_bytes!("build.rs").as_slice(),
            include_bytes!("build_support/cache.rs").as_slice(),
            include_bytes!("build_support/archive.rs").as_slice(),
            include_bytes!("build_support/registry.rs").as_slice(),
            include_bytes!("build_support/tags.rs").as_slice(),
            include_bytes!("assets/protocol/767/static_registry_ids.json").as_slice(),
        ]
        .concat(),
    );
    let version_cache = cache_root.join(cache::VERSION);
    let generated = version_cache.join(&fingerprint);
    println!(
        "cargo:rerun-if-changed={}",
        generated.join("registries.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        generated.join("manifest.json").display()
    );
    if let Some(path) = env::var_os("ZCRAFT_SERVER_JAR") {
        println!("cargo:rerun-if-changed={}", PathBuf::from(path).display());
    }
    let source = match cache::read_source(&generated, &fingerprint)? {
        Some(source) => source,
        None => {
            fs::create_dir_all(&version_cache)?;
            let jar = acquire_jar(&version_cache, offline)?;
            let extraction = tempfile::tempdir_in(&version_cache)?;
            archive::extract(&jar, extraction.path())?;
            let source = registry::generate(extraction.path())?;
            cache::publish(&generated, &fingerprint, &source)?;
            println!(
                "cargo:warning=Generated protocol 767 registries from verified official Minecraft {} data",
                cache::VERSION
            );
            source
        }
    };
    let out =
        PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("missing OUT_DIR"))?)
            .join("registries.rs");
    // Avoid rewriting unchanged output when only other tracked inputs changed.
    if fs::read(&out).ok().as_deref() != Some(source.as_bytes()) {
        cache::atomic_write(&out, source.as_bytes())?;
    }
    Ok(())
}

fn acquire_jar(directory: &Path, offline: bool) -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("ZCRAFT_SERVER_JAR") {
        let path = PathBuf::from(path);
        cache::verify_jar(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("local JAR {}: {error}", path.display()),
            )
        })?;
        return Ok(path);
    }
    let jar = directory.join("server.jar");
    if cache::verify_jar(&jar).is_ok() {
        return Ok(jar);
    }
    if offline {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "offline data build has neither a valid generated registry cache nor the verified official JAR",
        ));
    }
    println!(
        "cargo:warning=Registry cache missing; downloading official Minecraft {} server.jar ({} bytes)",
        cache::VERSION,
        cache::SERVER_BYTES
    );
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(io::Error::other)?;
    let response = client
        .get(SERVER_URL)
        .send()
        .map_err(io::Error::other)?
        .error_for_status()
        .map_err(io::Error::other)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(io::Error::other(format!(
            "official JAR download returned {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length != cache::SERVER_BYTES)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected official JAR Content-Length",
        ));
    }
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    let mut reader = response.take(cache::SERVER_BYTES + 1);
    let mut buffer = [0u8; 65536];
    let mut total = 0u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > cache::SERVER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "official JAR exceeds expected size",
            ));
        }
        file.write_all(&buffer[..count])?;
    }
    file.as_file().sync_all()?;
    cache::verify_jar(file.path())?;
    file.persist(&jar).map_err(|error| error.error)?;
    Ok(jar)
}
