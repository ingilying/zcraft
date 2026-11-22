use super::common::{ClientInformation, PluginMessage};
use crate::protocol::{Decode, Reader};
use std::io;

// Explicitly unsupported actions. Bound and discard;
// none can mutate the world, authenticate a player or broadcast chat.
macro_rules! ignored {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Debug, PartialEq, Eq)]
        pub struct $name;
        impl Decode for $name {
            fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
                reader.remaining(16384)?;
                Ok(Self)
            }
        }
    )*};
}
ignored!(
    IgnoredClientCommand,
    IgnoredCloseWindow,
    IgnoredAdvancementTab,
    IgnoredSpectate,
    IgnoredInventoryClick
);

#[derive(Debug, PartialEq, Eq)]
pub struct PlayClientInformation(pub ClientInformation);
impl Decode for PlayClientInformation {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self(ClientInformation::decode(r)?))
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct PlayPluginMessage(pub PluginMessage);
impl Decode for PlayPluginMessage {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self(PluginMessage::decode(r)?))
    }
}
