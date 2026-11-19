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
