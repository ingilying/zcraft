use std::io::{self, ErrorKind};

pub fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

pub fn put_varint(value: i32, out: &mut Vec<u8>) {
    let mut value = value as u32;
    loop {
        if value & !0x7f == 0 {
            out.push(value as u8);
            break;
        }
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
}

pub fn put_string(value: &str, out: &mut Vec<u8>) -> io::Result<()> {
    if value.len() > 32767 * 3 || value.encode_utf16().count() > 32767 {
        return Err(invalid("string too long"));
    }
    put_varint(value.len() as i32, out);
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

pub struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        if len > self.bytes.len() {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "truncated packet"));
        }
        let (head, tail) = self.bytes.split_at(len);
        self.bytes = tail;
        Ok(head)
    }

    pub fn varint(&mut self) -> io::Result<i32> {
        let mut value = 0u32;
        for i in 0..5 {
            let byte = self.take(1)?[0];
            if i == 4 && byte & 0xf0 != 0 {
                return Err(invalid("VarInt overflow"));
            }
            value |= u32::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value as i32);
            }
        }
        Err(invalid("VarInt too long"))
    }

    pub fn string(&mut self, max_chars: usize) -> io::Result<String> {
        let len = self.varint()?;
        if len < 0 || len as usize > max_chars * 3 {
            return Err(invalid("invalid string length"));
        }
        let value =
            std::str::from_utf8(self.take(len as usize)?).map_err(|_| invalid("invalid UTF-8"))?;
        if value.encode_utf16().count() > max_chars {
            return Err(invalid("string too long"));
        }
        Ok(value.to_owned())
    }

    pub fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn finish(self) -> io::Result<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(invalid("trailing packet data"))
        }
    }
}

pub trait Decode: Sized {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self>;
}

pub trait Encode {
    /// Encode fields only. The registry owns packet IDs.
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()>;
}
