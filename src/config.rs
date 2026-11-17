//! File configuration and validation, independent of packet dispatch.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    net::SocketAddr,
    path::Path,
    time::Duration,
};

pub const DEFAULT_CONFIG_PATH: &str = "zcraft.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub bind: String,
    pub motd: String,
    pub max_players: u32,
    pub max_connections: usize,
    /// Deadline for a complete frame or response, not per-byte activity.
    #[serde(rename = "io_timeout_seconds", with = "seconds")]
    pub io_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:25565".into(),
            motd: "ZCraft — Rust Minecraft Server".into(),
            max_players: 20,
            max_connections: 1024,
            io_timeout: Duration::from_secs(10),
        }
    }
}

impl Config {
    /// Read existing files unchanged; publish defaults atomically only if absent.
    pub fn load_or_create(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let result = (|| {
            let contents = match fs::read_to_string(path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // Do not replace dangling symlinks or other existing entries.
                    if fs::symlink_metadata(path).is_ok() {
                        return Err(error);
                    }
                    let defaults =
                        toml::to_string_pretty(&Self::default()).map_err(io::Error::other)?;
                    let contents = format!(
                        "# ZCraft server configuration. Restart after editing.\n\
                         # io_timeout_seconds is a complete read/write deadline (1..=86400).\n\n{defaults}"
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
                        // Another process created the file: use it, never overwrite it.
                        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                            fs::read_to_string(path)?
                        }
                        Err(error) => return Err(error.error),
                    }
                }
                Err(error) => return Err(error),
            };
            let config: Self = toml::from_str(&contents)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            config.validate()?;
            Ok(config)
        })();
        result.map_err(|error: io::Error| {
            io::Error::new(
                error.kind(),
                format!("configuration {}: {error}", path.display()),
            )
        })
    }

    /// Environment overrides are runtime-only; never persisted into the file.
    pub fn apply_overrides(
        &mut self,
        bind: Option<String>,
        motd: Option<String>,
    ) -> io::Result<()> {
        let mut updated = self.clone();
        if let Some(bind) = bind {
            updated.bind = bind;
        }
        if let Some(motd) = motd {
            updated.motd = motd;
        }
        updated.validate()?;
        *self = updated;
        Ok(())
    }

    pub fn validate(&self) -> io::Result<()> {
        let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
        self.bind
            .parse::<SocketAddr>()
            .map_err(|_| invalid("bind must be an IP:port, e.g. 0.0.0.0:25565 or [::]:25565"))?;
        if self.max_connections == 0 || self.max_connections > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(invalid(
                "max_connections is outside the supported semaphore range",
            ));
        }
        if self.io_timeout.is_zero() || self.io_timeout > Duration::from_secs(86400) {
            return Err(invalid(
                "io_timeout_seconds must be positive and at most 86400",
            ));
        }
        Ok(())
    }
}

mod seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        if value.subsec_nanos() != 0 {
            return Err(serde::ser::Error::custom(
                "file timeout must use whole seconds",
            ));
        }
        serializer.serialize_u64(value.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_secs)
    }
}
