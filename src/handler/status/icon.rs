//! Validated Minecraft server-list icon, encoded once at startup.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    fs::File,
    io::{self, Cursor, Read},
    path::Path,
};

/// Leaves room for other fields within the protocol's 32767-character JSON string.
pub const MAX_ICON_BYTES: usize = 20 * 1024;

#[derive(Debug)]
pub struct ServerIcon {
    data_uri: String,
}

impl ServerIcon {
    /// Missing files are optional; unreadable or invalid files are configuration errors.
    pub fn load_optional(path: impl AsRef<Path>) -> io::Result<Option<Self>> {
        let path = path.as_ref();
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(icon_error(path, error)),
        };
        let result = (|| {
            let mut bytes = Vec::new();
            file.take(MAX_ICON_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            Self::from_png(&bytes)
        })();
        result.map(Some).map_err(|error| icon_error(path, error))
    }

    pub fn from_png(bytes: &[u8]) -> io::Result<Self> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
        if bytes.len() > MAX_ICON_BYTES {
            return Err(invalid("PNG must not exceed 20 KiB".into()));
        }
        let decoder =
            png::Decoder::new_with_limits(Cursor::new(bytes), png::Limits { bytes: 1024 * 1024 });
        let mut reader = decoder
            .read_info()
            .map_err(|error| invalid(error.to_string()))?;
        let info = reader.info();
        if info.width != 64 || info.height != 64 {
            return Err(invalid("PNG dimensions must be exactly 64x64".into()));
        }
        if info.animation_control.is_some() {
            return Err(invalid("animated PNG icons are not supported".into()));
        }
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| invalid("PNG decoded size is too large".into()))?;
        let mut pixels = vec![0; size];
        reader
            .next_frame(&mut pixels)
            .map_err(|error| invalid(error.to_string()))?;
        reader
            .finish()
            .map_err(|error| invalid(error.to_string()))?;
        Ok(Self {
            data_uri: format!("data:image/png;base64,{}", STANDARD.encode(bytes)),
        })
    }

    pub fn data_uri(&self) -> &str {
        &self.data_uri
    }
}

fn icon_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("server icon {}: {error}", path.display()),
    )
}
