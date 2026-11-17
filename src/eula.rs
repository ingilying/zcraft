//! Explicit, fail-closed operator consent. Never writes an acceptance.
use std::{
    fs,
    io::{self, Write},
    path::Path,
};

pub const DEFAULT_EULA_PATH: &str = "eula.txt";
pub const EULA_URL: &str = "https://www.minecraft.net/eula";

/// Creates a refusal file if absent, then requires exactly one `eula=true`.
/// Call before binding any network listener.
pub fn require_acceptance(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let result = (|| {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Never replace existing entries, including dangling symlinks.
                if fs::symlink_metadata(path).is_ok() {
                    return Err(error);
                }
                let contents = format!(
                    "# Read the Minecraft EULA: {EULA_URL}\n\
                     # Set eula=true only if you agree to the EULA.\n\
                     eula=false\n"
                );
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                file.write_all(contents.as_bytes())?;
                file.as_file().sync_all()?;
                match file.persist_noclobber(path) {
                    Ok(_) => contents,
                    Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                        fs::read_to_string(path)?
                    }
                    Err(error) => return Err(error.error),
                }
            }
            Err(error) => return Err(error),
        };
        if !parse_acceptance(&contents)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("EULA not accepted. Read {EULA_URL}, then set eula=true only if you agree"),
            ));
        }
        Ok(())
    })();
    result.map_err(|error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("EULA file {}: {error}", path.display()),
        )
    })
}

fn parse_acceptance(contents: &str) -> io::Result<bool> {
    let mut accepted = None;
    for (index, line) in contents.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "line {}: expected a single eula=true or eula=false entry",
                    index + 1
                ),
            )
        };
        let (key, value) = line.split_once('=').ok_or_else(invalid)?;
        if key.trim() != "eula" || accepted.is_some() {
            return Err(invalid());
        }
        accepted = Some(match value.trim() {
            "true" => true,
            "false" => false,
            _ => return Err(invalid()),
        });
    }
    Ok(accepted.unwrap_or(false))
}
