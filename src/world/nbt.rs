//! Vanilla Minecraft Named Binary Tag (NBT) encoder/decoder with atomic GZip I/O.
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::Path,
};

const MAX_DEPTH: usize = 512;
const MAX_COLLECTION_LEN: usize = 16 * 1024 * 1024;

pub type Compound = BTreeMap<String, Tag>;

#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<i8>),
    String(String),
    List(Vec<Tag>),
    Compound(Compound),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Tag {
    fn id(&self) -> u8 {
        match self {
            Self::Byte(_) => 1,
            Self::Short(_) => 2,
            Self::Int(_) => 3,
            Self::Long(_) => 4,
            Self::Float(_) => 5,
            Self::Double(_) => 6,
            Self::ByteArray(_) => 7,
            Self::String(_) => 8,
            Self::List(_) => 9,
            Self::Compound(_) => 10,
            Self::IntArray(_) => 11,
            Self::LongArray(_) => 12,
        }
    }

    pub fn as_byte(&self) -> Option<i8> {
        match self {
            Self::Byte(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_short(&self) -> Option<i16> {
        match self {
            Self::Short(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i32> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_long(&self) -> Option<i64> {
        match self {
            Self::Long(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f32> {
        match self {
            Self::Float(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_double(&self) -> Option<f64> {
        match self {
            Self::Double(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(v) => Some(v.as_str()),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Tag]> {
        match self {
            Self::List(v) => Some(v.as_slice()),
            _ => None,
        }
    }

    pub fn as_compound(&self) -> Option<&Compound> {
        match self {
            Self::Compound(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_long_array(&self) -> Option<&[i64]> {
        match self {
            Self::LongArray(v) => Some(v.as_slice()),
            _ => None,
        }
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub fn write_named_nbt(root_name: &str, root: &Compound) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(256);
    out.push(10); // TAG_Compound
    write_string(&mut out, root_name)?;
    write_compound(&mut out, root, 0)?;
    Ok(out)
}

pub fn read_named_nbt(bytes: &[u8]) -> io::Result<(String, Compound)> {
    let mut reader = NbtReader(bytes);
    let tag_id = reader.u8()?;
    if tag_id != 10 {
        return Err(invalid(format!(
            "expected root TAG_Compound (10), got {tag_id}"
        )));
    }
    let name = reader.string()?;
    let compound = reader.compound(0)?;
    Ok((name, compound))
}

pub fn read_gzip_nbt(path: &Path) -> io::Result<(String, Compound)> {
    let file = fs::File::open(path)?;
    let mut decoder = GzDecoder::new(file);
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    read_named_nbt(&bytes)
}

pub fn write_gzip_nbt_atomic(path: &Path, root_name: &str, root: &Compound) -> io::Result<()> {
    let raw = write_named_nbt(root_name, root)?;
    let mut encoder = GzEncoder::new(Vec::with_capacity(raw.len() / 2), Compression::default());
    encoder.write_all(&raw)?;
    let compressed = encoder.finish()?;
    write_bytes_atomic(path, &compressed)
}

pub fn write_bytes_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn write_string(out: &mut Vec<u8>, s: &str) -> io::Result<()> {
    let len = u16::try_from(s.len()).map_err(|_| invalid("NBT string exceeds 65535 bytes"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn write_compound(out: &mut Vec<u8>, map: &Compound, depth: usize) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(invalid("NBT nesting depth exceeded"));
    }
    for (name, tag) in map {
        out.push(tag.id());
        write_string(out, name)?;
        write_payload(out, tag, depth + 1)?;
    }
    out.push(0); // TAG_End
    Ok(())
}

fn write_payload(out: &mut Vec<u8>, tag: &Tag, depth: usize) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(invalid("NBT nesting depth exceeded"));
    }
    match tag {
        Tag::Byte(v) => out.push(*v as u8),
        Tag::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Float(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Double(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::ByteArray(arr) => {
            let len = i32::try_from(arr.len()).map_err(|_| invalid("ByteArray too large"))?;
            out.extend_from_slice(&len.to_be_bytes());
            for &b in arr {
                out.push(b as u8);
            }
        }
        Tag::String(s) => write_string(out, s)?,
        Tag::List(items) => {
            let elem_id = items.first().map_or(0, Tag::id);
            out.push(elem_id);
            let len = i32::try_from(items.len()).map_err(|_| invalid("List too large"))?;
            out.extend_from_slice(&len.to_be_bytes());
            for item in items {
                if item.id() != elem_id {
                    return Err(invalid("heterogeneous NBT list"));
                }
                write_payload(out, item, depth + 1)?;
            }
        }
        Tag::Compound(map) => write_compound(out, map, depth)?,
        Tag::IntArray(arr) => {
            let len = i32::try_from(arr.len()).map_err(|_| invalid("IntArray too large"))?;
            out.extend_from_slice(&len.to_be_bytes());
            for &v in arr {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        Tag::LongArray(arr) => {
            let len = i32::try_from(arr.len()).map_err(|_| invalid("LongArray too large"))?;
            out.extend_from_slice(&len.to_be_bytes());
            for &v in arr {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
    }
    Ok(())
}

struct NbtReader<'a>(&'a [u8]);

impl<'a> NbtReader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated NBT payload",
            ));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> io::Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn f32(&mut self) -> io::Result<f32> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn f64(&mut self) -> io::Result<f64> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self) -> io::Result<String> {
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| invalid("invalid UTF-8 in NBT string"))
    }

    fn len(&mut self) -> io::Result<usize> {
        let raw = self.i32()?;
        if raw < 0 {
            return Ok(0);
        }
        let n = raw as usize;
        if n > MAX_COLLECTION_LEN {
            return Err(invalid("NBT collection exceeds size limit"));
        }
        Ok(n)
    }

    fn compound(&mut self, depth: usize) -> io::Result<Compound> {
        if depth > MAX_DEPTH {
            return Err(invalid("NBT nesting depth exceeded"));
        }
        let mut map = Compound::new();
        loop {
            let tag_id = self.u8()?;
            if tag_id == 0 {
                break;
            }
            let name = self.string()?;
            let value = self.payload(tag_id, depth + 1)?;
            map.insert(name, value);
        }
        Ok(map)
    }

    fn payload(&mut self, tag_id: u8, depth: usize) -> io::Result<Tag> {
        if depth > MAX_DEPTH {
            return Err(invalid("NBT nesting depth exceeded"));
        }
        match tag_id {
            1 => Ok(Tag::Byte(self.u8()? as i8)),
            2 => Ok(Tag::Short(self.i16()?)),
            3 => Ok(Tag::Int(self.i32()?)),
            4 => Ok(Tag::Long(self.i64()?)),
            5 => Ok(Tag::Float(self.f32()?)),
            6 => Ok(Tag::Double(self.f64()?)),
            7 => {
                let len = self.len()?;
                let bytes = self.take(len)?;
                Ok(Tag::ByteArray(bytes.iter().map(|&b| b as i8).collect()))
            }
            8 => Ok(Tag::String(self.string()?)),
            9 => {
                let elem_id = self.u8()?;
                let len = self.len()?;
                if elem_id == 0 && len > 0 {
                    return Err(invalid("non-empty NBT list with TAG_End element type"));
                }
                let mut items = Vec::with_capacity(len.min(1024));
                for _ in 0..len {
                    items.push(self.payload(elem_id, depth + 1)?);
                }
                Ok(Tag::List(items))
            }
            10 => Ok(Tag::Compound(self.compound(depth)?)),
            11 => {
                let len = self.len()?;
                let mut arr = Vec::with_capacity(len.min(1024));
                for _ in 0..len {
                    arr.push(self.i32()?);
                }
                Ok(Tag::IntArray(arr))
            }
            12 => {
                let len = self.len()?;
                let mut arr = Vec::with_capacity(len.min(1024));
                for _ in 0..len {
                    arr.push(self.i64()?);
                }
                Ok(Tag::LongArray(arr))
            }
            other => Err(invalid(format!("unknown NBT tag type {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_tag_types_and_gzip_file() {
        let mut inner = Compound::new();
        inner.insert("nested".into(), Tag::String("ok".into()));

        let mut root = Compound::new();
        root.insert("byte".into(), Tag::Byte(-7));
        root.insert("short".into(), Tag::Short(1234));
        root.insert("int".into(), Tag::Int(3955));
        root.insert("long".into(), Tag::Long(-987_654_321_000));
        root.insert("float".into(), Tag::Float(1.5));
        root.insert("double".into(), Tag::Double(-42.25));
        root.insert("bytes".into(), Tag::ByteArray(vec![1, -2, 3]));
        root.insert("str".into(), Tag::String("minecraft:overworld".into()));
        root.insert(
            "list".into(),
            Tag::List(vec![Tag::Int(10), Tag::Int(20), Tag::Int(30)]),
        );
        root.insert("empty_list".into(), Tag::List(vec![]));
        root.insert("compound".into(), Tag::Compound(inner));
        root.insert("ints".into(), Tag::IntArray(vec![-1, 0, 1]));
        root.insert("longs".into(), Tag::LongArray(vec![i64::MIN, 0, i64::MAX]));

        let bytes = write_named_nbt("Root", &root).unwrap();
        let (name, decoded) = read_named_nbt(&bytes).unwrap();
        assert_eq!(name, "Root");
        assert_eq!(decoded, root);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("level.dat");
        write_gzip_nbt_atomic(&path, "", &root).unwrap();
        let (gz_name, gz_decoded) = read_gzip_nbt(&path).unwrap();
        assert_eq!(gz_name, "");
        assert_eq!(gz_decoded, root);
    }
}
