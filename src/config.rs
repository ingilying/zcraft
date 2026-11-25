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
pub const DEFAULT_COMPRESSION_THRESHOLD: i32 = 256;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GameMode {
    #[default]
    Spectator,
    Creative,
}

impl GameMode {
    pub(crate) fn wire_id(self) -> u8 {
        match self {
            Self::Creative => 1,
            Self::Spectator => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub bind: String,
    pub motd: String,
    /// Relative paths are resolved from the working directory.
    pub server_icon: String,
    pub max_players: u32,
    /// Maximum per-player chunk radius (2–16). Smaller client settings win.
    pub max_view_distance: i32,
    pub game_mode: GameMode,
    /// Disables Mojang verification. Unsafe on untrusted networks.
    pub offline_mode: bool,
    pub max_connections: usize,
    /// Packet body size at which zlib compression starts; -1 disables it.
    pub compression_threshold: i32,
    /// Deadline for a complete frame or response, not per-byte activity.
    #[serde(rename = "io_timeout_seconds", with = "seconds")]
    pub io_timeout: Duration,
    /// Optional vanilla resource pack, negotiated before entering Play.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_pack: Option<ResourcePackConfig>,
    /// Operator usernames or UUIDs granted command permission level 4.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<String>,
    /// Optional Anvil world directory (level.dat, region/*.mca, playerdata/*.dat).
    /// When omitted, the world runs in memory only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_dir: Option<String>,
    /// Optional Superflat generator preset (used when creating a new world or in memory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flat: Option<crate::world::FlatGeneratorConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcePackConfig {
    pub id: String,
    pub url: String,
    pub sha1: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

impl ResourcePackConfig {
    pub fn validate(&self) -> io::Result<()> {
        let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
        uuid::Uuid::parse_str(&self.id).map_err(|_| invalid("resource_pack.id must be a UUID"))?;
        let url = reqwest::Url::parse(&self.url)
            .map_err(|_| invalid("resource_pack.url must be an HTTP(S) URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || self.url.encode_utf16().count() > 32767
        {
            return Err(invalid(
                "resource_pack.url must be an HTTP(S) URL of at most 32767 characters",
            ));
        }
        if self.sha1.len() != 40 || !self.sha1.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("resource_pack.sha1 must be 40 hexadecimal digits"));
        }
        if self.prompt.as_ref().is_some_and(|prompt| {
            // Text components use Java modified UTF-8 in anonymous NBT.
            prompt
                .chars()
                .map(|c| {
                    if c == '\0' {
                        2
                    } else if c as u32 > 0xffff {
                        6
                    } else {
                        c.len_utf8()
                    }
                })
                .sum::<usize>()
                > 65535
        }) {
            return Err(invalid("resource_pack.prompt exceeds the NBT string limit"));
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:25565".into(),
            motd: "ZCraft — Rust Minecraft Server".into(),
            server_icon: "server-icon.png".into(),
            max_players: 20,
            max_view_distance: crate::world::VIEW_DISTANCE,
            game_mode: GameMode::default(),
            offline_mode: false,
            max_connections: 1024,
            compression_threshold: DEFAULT_COMPRESSION_THRESHOLD,
            io_timeout: Duration::from_secs(10),
            resource_pack: None,
            ops: Vec::new(),
            world_dir: None,
            flat: None,
        }
    }
}

impl Config {
    fn default_toml() -> io::Result<String> {
        #[derive(Serialize)]
        struct OptionalDefaults {
            resource_pack: ResourcePackConfig,
        }

        let defaults = toml::to_string_pretty(&Self::default()).map_err(io::Error::other)?;
        let optional = toml::to_string_pretty(&OptionalDefaults {
            resource_pack: ResourcePackConfig {
                id: "00000000-0000-0000-0000-000000000001".into(),
                url: "https://example.org/pack.zip".into(),
                sha1: "0123456789012345678901234567890123456789".into(),
                required: false,
                prompt: Some("Load server resource pack?".into()),
            },
        })
        .map_err(io::Error::other)?;
        let commented_optional = optional
            .lines()
            .map(|line| format!("# {line}\n"))
            .collect::<String>();
        Ok(format!(
            "# ZCraft server configuration. Restart after editing.\n\
             # io_timeout_seconds is a complete read/write deadline (1..=86400).\n\
             # compression_threshold is in bytes; -1 disables compression.\n\n\
             {defaults}\n\
             # Optional operator usernames or UUIDs (permission level 4):\n\
             # ops = [\"YourName\"]\n\n\
             # Optional Anvil world persistence directory (level.dat, region/*.mca, playerdata/*.dat):\n\
             # world_dir = \"world\"\n\n\
             # Optional Superflat terrain preset (default: y=60 bedrock, 2x dirt, grass_block):\n\
             # [flat]\n\
             # base_y = -64\n\
             # biome = \"minecraft:plains\"\n\
             # layers = [\n\
             #   {{ block = \"minecraft:bedrock\", height = 1 }},\n\
             #   {{ block = \"minecraft:dirt\", height = 2 }},\n\
             #   {{ block = \"minecraft:grass_block\", height = 1 }},\n\
             # ]\n\n\
             # Optional server resource pack (uncomment and configure to enable):\n\
             {commented_optional}"
        ))
    }

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
                    let contents = Self::default_toml()?;
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
        if !(-1..=crate::protocol::MAX_FRAME_SIZE as i32).contains(&self.compression_threshold) {
            return Err(invalid("compression_threshold must be -1..=2097151"));
        }
        crate::world::validate_view_distance(self.max_view_distance)?;
        if let Some(pack) = &self.resource_pack {
            pack.validate()?;
        }
        for entry in &self.ops {
            let valid_uuid = uuid::Uuid::parse_str(entry).is_ok();
            if !valid_uuid && !crate::auth::valid_username(entry) {
                return Err(invalid("ops entries must be valid usernames or UUIDs"));
            }
        }
        if let Some(dir) = &self.world_dir
            && dir.trim().is_empty()
        {
            return Err(invalid("world_dir must not be empty"));
        }
        if let Some(flat) = &self.flat {
            flat.validate()?;
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
