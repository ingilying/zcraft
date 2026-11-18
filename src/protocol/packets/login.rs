use crate::{
    identity::Profile,
    protocol::{Decode, Encode, Reader, invalid, put_string, put_varint},
};
use std::io;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq)]
pub struct LoginStart {
    pub name: String,
    /// Parsed for protocol compatibility, never trusted as an authenticated identity.
    pub claimed_uuid: Uuid,
}

impl Decode for LoginStart {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            name: reader.string(16)?,
            claimed_uuid: Uuid::from_bytes(reader.take(16)?.try_into().unwrap()),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EncryptionResponse {
    pub shared_secret: Vec<u8>,
    pub verify_token: Vec<u8>,
}

impl Decode for EncryptionResponse {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            shared_secret: reader.byte_array(128)?,
            verify_token: reader.byte_array(128)?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct LoginAcknowledged;

impl Decode for LoginAcknowledged {
    fn decode(_reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct LoginDisconnect {
    pub reason: String,
}

impl Encode for LoginDisconnect {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_string(&self.reason, out)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EncryptionRequest {
    pub public_key: Vec<u8>,
    pub verify_token: [u8; 4],
}

impl Encode for EncryptionRequest {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_string("", out)?; // Modern servers use an empty server ID.
        for bytes in [&self.public_key[..], &self.verify_token[..]] {
            put_varint(bytes.len() as i32, out);
            out.extend_from_slice(bytes);
        }
        out.push(1); // shouldAuthenticate (protocol 767)
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct LoginSuccess {
    pub profile: Profile,
}

impl Encode for LoginSuccess {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        if self.profile.properties.len() > 16 {
            return Err(invalid("too many profile properties"));
        }
        out.extend_from_slice(self.profile.uuid.as_bytes());
        put_string(&self.profile.name, out)?;
        put_varint(self.profile.properties.len() as i32, out);
        for property in &self.profile.properties {
            put_string(&property.name, out)?;
            put_string(&property.value, out)?;
            out.push(u8::from(property.signature.is_some()));
            if let Some(signature) = &property.signature {
                put_string(signature, out)?;
            }
        }
        out.push(1); // strictErrorHandling (1.21/767)
        Ok(())
    }
}
