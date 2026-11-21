use crate::protocol::{Decode, Reader, invalid};
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInformation {
    pub locale: String,
    pub view_distance: i8,
    pub chat_mode: i32,
    pub chat_colors: bool,
    pub skin_parts: u8,
    pub main_hand: i32,
    pub text_filtering: bool,
    pub server_listing: bool,
}

/// An anonymous TAG_String text component using Java modified UTF-8.
pub(super) fn put_nbt_text(text: &str, out: &mut Vec<u8>) -> io::Result<()> {
    // Modified UTF-8 never uses fewer bytes than standard UTF-8.
    if text.len() > u16::MAX as usize {
        return Err(invalid("text exceeds NBT string limit"));
    }
    let mut bytes = Vec::with_capacity(text.len());
    for unit in text.encode_utf16() {
        match unit {
            1..=0x7f => bytes.push(unit as u8),
            0..=0x7ff => {
                bytes.push(0xc0 | (unit >> 6) as u8);
                bytes.push(0x80 | (unit & 0x3f) as u8);
            }
            _ => {
                bytes.push(0xe0 | (unit >> 12) as u8);
                bytes.push(0x80 | ((unit >> 6) & 0x3f) as u8);
                bytes.push(0x80 | (unit & 0x3f) as u8);
            }
        }
    }
    let length =
        u16::try_from(bytes.len()).map_err(|_| invalid("text exceeds NBT string limit"))?;
    out.push(8);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&bytes);
    Ok(())
}

fn boolean(reader: &mut Reader<'_>) -> io::Result<bool> {
    match reader.take(1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid boolean")),
    }
}

impl Decode for ClientInformation {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let locale = reader.string(16)?;
        let view_distance = reader.take(1)?[0] as i8;
        let chat_mode = reader.varint()?;
        let chat_colors = boolean(reader)?;
        let skin_parts = reader.take(1)?[0];
        let main_hand = reader.varint()?;
        if !(0..=2).contains(&chat_mode) || !(0..=1).contains(&main_hand) || view_distance < 2 {
            return Err(invalid("invalid client settings"));
        }
        Ok(Self {
            locale,
            view_distance,
            chat_mode,
            chat_colors,
            skin_parts,
            main_hand,
            text_filtering: boolean(reader)?,
            server_listing: boolean(reader)?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PluginMessage {
    pub channel: String,
    pub data: Vec<u8>,
}

impl Decode for PluginMessage {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            channel: reader.string(32767)?,
            data: reader.remaining(32767)?,
        })
    }
}
