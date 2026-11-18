use crate::protocol::Encode;
use std::io;

/// Protocol 767 uses anonymous network NBT for text components, not JSON.
/// A root TAG_String is sufficient for our fixed, ASCII disconnect message.
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigurationDisconnect;

pub const NOT_IMPLEMENTED: &str = "ZCraft: login successful; world entry is not implemented yet.";

impl Encode for ConfigurationDisconnect {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(8); // Anonymous TAG_String (no root name).
        out.extend_from_slice(&(NOT_IMPLEMENTED.len() as u16).to_be_bytes());
        out.extend_from_slice(NOT_IMPLEMENTED.as_bytes());
        Ok(())
    }
}
