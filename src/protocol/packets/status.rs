use crate::protocol::{Decode, Encode, Reader, put_string};
use std::io;

#[derive(Debug, PartialEq, Eq)]
pub struct StatusRequest;

impl Decode for StatusRequest {
    fn decode(_reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Ping {
    pub payload: i64,
}

impl Decode for Ping {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            payload: reader.i64()?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct StatusResponse {
    pub json: String,
}

impl Encode for StatusResponse {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_string(&self.json, out)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Pong {
    pub payload: i64,
}

impl Encode for Pong {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(&self.payload.to_be_bytes());
        Ok(())
    }
}
