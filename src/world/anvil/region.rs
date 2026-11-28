//! Anvil `region/r.<rx>.<rz>.mca` 32x32 chunk region file binary layout & I/O.
use super::{
    AnvilStorage,
    chunk_codec::{decode_chunk_anvil, encode_chunk_anvil},
};
use crate::world::{Chunk, nbt::write_bytes_atomic};
use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const SECTOR_BYTES: usize = 4096;
const REGION_CHUNKS: usize = 1024;
const HEADER_BYTES: usize = SECTOR_BYTES * 2;

impl AnvilStorage {
    fn region_dir(&self) -> PathBuf {
        self.root.join("region")
    }

    fn region_path(&self, rx: i32, rz: i32) -> PathBuf {
        self.region_dir().join(format!("r.{rx}.{rz}.mca"))
    }

    /// Lazily load a single chunk from its `region/r.<rx>.<rz>.mca` file.
    /// Returns `Ok(None)` if the region file or chunk slot does not exist.
    pub fn load_chunk(&self, cx: i32, cz: i32) -> io::Result<Option<Chunk>> {
        let rx = cx.div_euclid(32);
        let rz = cz.div_euclid(32);
        let path = self.region_path(rx, rz);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        if bytes.len() < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated Anvil region header",
            ));
        }
        let slot = region_slot(cx, cz);
        let loc = u32::from_be_bytes(bytes[slot * 4..slot * 4 + 4].try_into().unwrap());
        let offset = (loc >> 8) as usize;
        let sectors = (loc & 0xff) as usize;
        if offset == 0 || sectors == 0 {
            return Ok(None);
        }
        let start = offset * SECTOR_BYTES;
        let end_sector = start + sectors * SECTOR_BYTES;
        if offset < 2 || end_sector > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Anvil chunk sector out of bounds",
            ));
        }
        let length = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
        if length == 0 || start + 4 + length > end_sector {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Anvil chunk length",
            ));
        }
        let payload = &bytes[start + 4..start + 4 + length];
        let chunk = decode_chunk_anvil(payload)?;
        Ok(Some(chunk))
    }

    /// Save a batch of dirty chunks grouped by region file (`r.<rx>.<rz>.mca`).
    pub fn save_chunks(&self, chunks: &[([i32; 2], Arc<Chunk>)]) -> io::Result<usize> {
        if chunks.is_empty() {
            return Ok(0);
        }
        type RegionUpdate<'a> = ([i32; 2], &'a Chunk);
        let mut by_region: BTreeMap<[i32; 2], Vec<RegionUpdate<'_>>> = BTreeMap::new();
        for (coords, chunk) in chunks {
            let region = [coords[0].div_euclid(32), coords[1].div_euclid(32)];
            by_region
                .entry(region)
                .or_default()
                .push((*coords, chunk.as_ref()));
        }
        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        let mut saved = 0usize;
        for ([rx, rz], updates) in by_region {
            let path = self.region_path(rx, rz);
            let mut region = if path.exists() {
                RegionFile::parse(&fs::read(&path)?)?
            } else {
                RegionFile::default()
            };
            for ([cx, cz], chunk) in updates {
                let slot = region_slot(cx, cz);
                let payload = encode_chunk_anvil(cx, cz, chunk)?;
                region.chunks[slot] = Some((payload, now_sec));
                saved += 1;
            }
            let encoded = region.encode();
            write_bytes_atomic(&path, &encoded)?;
        }
        Ok(saved)
    }
}

fn region_slot(cx: i32, cz: i32) -> usize {
    cx.rem_euclid(32) as usize + cz.rem_euclid(32) as usize * 32
}

type RegionSlot = Option<(Vec<u8>, u32)>;

struct RegionFile {
    chunks: Box<[RegionSlot; REGION_CHUNKS]>,
}

impl Default for RegionFile {
    fn default() -> Self {
        Self {
            chunks: Box::new(std::array::from_fn(|_| None)),
        }
    }
}

impl RegionFile {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        if bytes.len() < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated Anvil region header",
            ));
        }
        let mut region = Self::default();
        for slot in 0..REGION_CHUNKS {
            let loc = u32::from_be_bytes(bytes[slot * 4..slot * 4 + 4].try_into().unwrap());
            let offset = (loc >> 8) as usize;
            let sectors = (loc & 0xff) as usize;
            if offset == 0 || sectors == 0 {
                continue;
            }
            let ts = u32::from_be_bytes(
                bytes[SECTOR_BYTES + slot * 4..SECTOR_BYTES + slot * 4 + 4]
                    .try_into()
                    .unwrap(),
            );
            let start = offset * SECTOR_BYTES;
            let end_sector = start + sectors * SECTOR_BYTES;
            if offset < 2 || end_sector > bytes.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Anvil chunk sector out of bounds",
                ));
            }
            let length = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
            if length == 0 || start + 4 + length > end_sector {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Anvil chunk length",
                ));
            }
            let payload = bytes[start + 4..start + 4 + length].to_vec();
            region.chunks[slot] = Some((payload, ts));
        }
        Ok(region)
    }

    fn encode(&self) -> Vec<u8> {
        let mut header = vec![0u8; HEADER_BYTES];
        let mut body = Vec::new();
        let mut next_sector: u32 = 2;

        for (slot, entry) in self.chunks.iter().enumerate() {
            let Some((payload, ts)) = entry else {
                continue;
            };
            let total_bytes = 4 + payload.len();
            let sectors = total_bytes.div_ceil(SECTOR_BYTES);
            let sectors_u8 = u8::try_from(sectors).expect("chunk fits within 255 sectors");
            let loc = (next_sector << 8) | u32::from(sectors_u8);
            header[slot * 4..slot * 4 + 4].copy_from_slice(&loc.to_be_bytes());
            header[SECTOR_BYTES + slot * 4..SECTOR_BYTES + slot * 4 + 4]
                .copy_from_slice(&ts.to_be_bytes());

            body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            body.extend_from_slice(payload);
            let padded_len = sectors * SECTOR_BYTES;
            body.resize(body.len() + (padded_len - total_bytes), 0);
            next_sector += u32::from(sectors_u8);
        }

        let mut out = Vec::with_capacity(header.len() + body.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&body);
        out
    }
}
