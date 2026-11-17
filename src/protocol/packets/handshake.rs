use crate::protocol::{Decode, Reader, State, invalid};
use std::io;

#[derive(Debug, PartialEq, Eq)]
pub struct Handshake {
    pub protocol: i32,
    pub address: String,
    pub port: u16,
    pub next: State,
}

impl Decode for Handshake {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let protocol = reader.varint()?;
        let address = reader.string(255)?;
        let port = reader.u16()?;
        let next = match reader.varint()? {
            1 => State::Status,
            2 | 3 => State::Login, // Login and transfer are currently rejected.
            _ => return Err(invalid("invalid handshake next state")),
        };
        Ok(Self {
            protocol,
            address,
            port,
            next,
        })
    }
}
