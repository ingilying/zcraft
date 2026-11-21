use super::common::{ClientInformation, PluginMessage};
use crate::protocol::{Decode, Encode, Reader, invalid, put_string, put_varint};
use std::{io, sync::Arc};

#[derive(Debug, PartialEq, Eq)]
pub struct ConfigurationClientInformation(pub ClientInformation);
impl Decode for ConfigurationClientInformation {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self(ClientInformation::decode(r)?))
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigurationPluginMessage(pub PluginMessage);
impl Decode for ConfigurationPluginMessage {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self(PluginMessage::decode(r)?))
    }
}

/// No packs are advertised, so the only legitimate selection is empty.
/// All registry entries are sent in full instead of trusting local pack data.
#[derive(Debug, PartialEq, Eq)]
pub struct KnownPacksResponse;
impl Decode for KnownPacksResponse {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        if r.varint()? != 0 {
            return Err(invalid("client selected unadvertised known packs"));
        }
        Ok(Self)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct FinishConfigurationAcknowledged;
impl Decode for FinishConfigurationAcknowledged {
    fn decode(_r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConfigurationKeepAliveResponse {
    pub id: i64,
}
impl Decode for ConfigurationKeepAliveResponse {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self { id: r.i64()? })
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigurationKeepAlive {
    pub id: i64,
}
impl Encode for ConfigurationKeepAlive {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(&self.id.to_be_bytes());
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourcePackStatus {
    SuccessfullyLoaded,
    Declined,
    FailedDownload,
    Accepted,
    Downloaded,
    InvalidUrl,
    FailedReload,
    Discarded,
}
#[derive(Debug, PartialEq, Eq)]
pub struct ResourcePackResponse {
    pub id: uuid::Uuid,
    pub status: ResourcePackStatus,
}
impl Decode for ResourcePackResponse {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        let id = uuid::Uuid::from_bytes(r.take(16)?.try_into().unwrap());
        let status = match r.varint()? {
            0 => ResourcePackStatus::SuccessfullyLoaded,
            1 => ResourcePackStatus::Declined,
            2 => ResourcePackStatus::FailedDownload,
            3 => ResourcePackStatus::Accepted,
            4 => ResourcePackStatus::Downloaded,
            5 => ResourcePackStatus::InvalidUrl,
            6 => ResourcePackStatus::FailedReload,
            7 => ResourcePackStatus::Discarded,
            _ => return Err(invalid("invalid resource pack status")),
        };
        Ok(Self { id, status })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResourcePack {
    pub id: uuid::Uuid,
    pub url: String,
    pub sha1: String,
    pub required: bool,
    pub prompt: Option<String>,
}
impl Encode for AddResourcePack {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(self.id.as_bytes());
        put_string(&self.url, out)?;
        put_string(&self.sha1, out)?;
        out.push(u8::from(self.required));
        out.push(u8::from(self.prompt.is_some()));
        if let Some(prompt) = &self.prompt {
            super::common::put_nbt_text(prompt, out)?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct StartConfiguration;
impl Encode for StartConfiguration {
    fn encode(&self, _out: &mut Vec<u8>) -> io::Result<()> {
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigurationAcknowledged;
impl Decode for ConfigurationAcknowledged {
    fn decode(_r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FeatureFlags;
impl Encode for FeatureFlags {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(1, out);
        put_string("minecraft:vanilla", out)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct SelectKnownPacks;
impl Encode for SelectKnownPacks {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(0, out);
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct RegistryData(pub Arc<[u8]>);
impl Encode for RegistryData {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(&self.0);
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct UpdateTags(pub Arc<[u8]>);
impl Encode for UpdateTags {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(&self.0);
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct FinishConfiguration;
impl Encode for FinishConfiguration {
    fn encode(&self, _out: &mut Vec<u8>) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConfigurationDisconnect {
    pub reason: String,
}
impl Encode for ConfigurationDisconnect {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        super::play::PlayDisconnect {
            reason: self.reason.clone(),
        }
        .encode(out)
    }
}
