//! Protocol 767 mappings. Each direction has its own ID namespace.
//! Macros generate static matches, not heap-allocated trait objects.
use super::{
    Decode, Encode, Reader, State, invalid,
    packets::{
        configuration::*,
        handshake::Handshake,
        login::{
            EncryptionRequest, EncryptionResponse, LoginAcknowledged, LoginDisconnect, LoginStart,
            LoginSuccess, SetCompression,
        },
        play::*,
        play_common::*,
        play_entities::*,
        status::{Ping, Pong, StatusRequest, StatusResponse},
    },
    put_varint,
};
use crate::handler::{Context, Outcome};
use std::io;

const fn assert_unique(entries: &[(State, i32)]) {
    let mut i = 0;
    while i < entries.len() {
        assert!(entries[i].1 >= 0, "packet ID must be non-negative");
        let mut j = i + 1;
        while j < entries.len() {
            assert!(
                entries[i].0 as u8 != entries[j].0 as u8 || entries[i].1 != entries[j].1,
                "duplicate packet ID in the same state and direction"
            );
            j += 1;
        }
        i += 1;
    }
}

macro_rules! serverbound_packets {
    ($( $state:ident { $( $id:literal => $variant:ident($packet:ty) => $handler:path ),* $(,)? } )*) => {
        const _: () = assert_unique(&[$($( (State::$state, $id), )*)*]);

        #[derive(Debug, PartialEq)]
        pub enum Serverbound {
            $($( $variant($packet), )*)*
        }

        $($(
            impl From<$packet> for Serverbound {
                fn from(packet: $packet) -> Self { Self::$variant(packet) }
            }
        )*)*

        pub fn decode(state: State, body: &[u8]) -> io::Result<Serverbound> {
            let mut reader = Reader::new(body);
            let id = reader.varint()?;
            let packet = match (state, id) {
                $($( (State::$state, $id) => Serverbound::$variant(
                    <$packet as Decode>::decode(&mut reader)?
                ), )*)*
                _ => return Err(invalid("unknown packet ID for connection state")),
            };
            // Always validate before running any business logic.
            reader.finish()?;
            Ok(packet)
        }

        impl Serverbound {
            pub fn state(&self) -> State {
                match self {
                    $($( Self::$variant(_) => State::$state, )*)*
                }
            }

            pub(crate) async fn dispatch(self, ctx: &mut Context<'_>) -> io::Result<Outcome> {
                // Also guards packets constructed directly rather than decoded from TCP.
                if ctx.session.state != self.state() {
                    return Err(invalid("packet not allowed in current session"));
                }
                match self {
                    $($( Self::$variant(packet) => $handler(ctx, packet).await, )*)*
                }
            }
        }
    };
}

macro_rules! clientbound_packets {
    ($( $state:ident { $( $id:literal => $variant:ident($packet:ty) ),* $(,)? } )*) => {
        const _: () = assert_unique(&[$($( (State::$state, $id), )*)*]);

        #[derive(Debug, PartialEq, Eq)]
        pub enum Clientbound {
            $($( $variant($packet), )*)*
        }

        $($(
            impl From<$packet> for Clientbound {
                fn from(packet: $packet) -> Self { Self::$variant(packet) }
            }
        )*)*

        impl Clientbound {
            pub fn state(&self) -> State {
                match self {
                    $($( Self::$variant(_) => State::$state, )*)*
                }
            }

            pub fn encode(&self) -> io::Result<Vec<u8>> {
                let mut out = Vec::new();
                match self {
                    $($( Self::$variant(packet) => {
                        put_varint($id, &mut out);
                        <$packet as Encode>::encode(packet, &mut out)?;
                    }, )*)*
                }
                Ok(out)
            }
        }
    };
}

// Add a packet here after defining its fields/codec and typed handler.
serverbound_packets! {
    Handshake {
        0x00 => Handshake(Handshake) => crate::handler::handshake::handle_handshake,
    }
    Status {
        0x00 => StatusRequest(StatusRequest) => crate::handler::status::handle_status_request,
        0x01 => Ping(Ping) => crate::handler::status::handle_ping,
    }
    Login {
        0x00 => LoginStart(LoginStart) => crate::handler::login::handle_start,
        0x01 => EncryptionResponse(EncryptionResponse) => crate::handler::login::handle_encryption_response,
        0x03 => LoginAcknowledged(LoginAcknowledged) => crate::handler::login::handle_acknowledged,
    }
    Configuration {
        0x00 => ConfigurationClientInformation(ConfigurationClientInformation) => crate::handler::configuration::handle_information,
        0x02 => ConfigurationPluginMessage(ConfigurationPluginMessage) => crate::handler::configuration::handle_plugin,
        0x03 => FinishConfigurationAcknowledged(FinishConfigurationAcknowledged) => crate::handler::configuration::handle_finish,
        0x04 => ConfigurationKeepAliveResponse(ConfigurationKeepAliveResponse) => crate::handler::configuration::handle_keep_alive,
        0x06 => ResourcePackResponse(ResourcePackResponse) => crate::handler::configuration::handle_resource_pack,
        0x07 => KnownPacksResponse(KnownPacksResponse) => crate::handler::configuration::handle_known_packs,
    }
    Play {
        0x00 => TeleportConfirm(TeleportConfirm) => crate::handler::play::handle_teleport,
        0x04 => IgnoredChatCommand(IgnoredChatCommand) => crate::handler::play::ignore,
        0x05 => IgnoredSignedCommand(IgnoredSignedCommand) => crate::handler::play::ignore,
        0x06 => IgnoredChatMessage(IgnoredChatMessage) => crate::handler::play::ignore,
        0x07 => ChatSessionUpdate(ChatSessionUpdate) => crate::handler::play::ignore,
        0x08 => ChunkBatchReceived(ChunkBatchReceived) => crate::handler::play::handle_batch,
        0x09 => IgnoredClientCommand(IgnoredClientCommand) => crate::handler::play::ignore,
        0x0a => PlayClientInformation(PlayClientInformation) => crate::handler::play::handle_information,
        0x0b => IgnoredTabComplete(IgnoredTabComplete) => crate::handler::play::ignore,
        0x0c => ConfigurationAcknowledged(ConfigurationAcknowledged) => crate::handler::configuration::handle_reconfiguration_acknowledged,
        0x0f => IgnoredCloseWindow(IgnoredCloseWindow) => crate::handler::play::ignore,
        0x12 => PlayPluginMessage(PlayPluginMessage) => crate::handler::play::ignore,
        0x18 => KeepAliveResponse(KeepAliveResponse) => crate::handler::play::handle_keep_alive,
        0x1a => Position(Position) => crate::handler::play::handle_position,
        0x1b => PositionLook(PositionLook) => crate::handler::play::handle_position_look,
        0x1c => Look(Look) => crate::handler::play::handle_look,
        0x1d => Flying(Flying) => crate::handler::play::handle_flying,
        0x23 => Abilities(Abilities) => crate::handler::play::ignore,
        0x24 => IgnoredPlayerDigging(IgnoredPlayerDigging) => crate::handler::play::ignore,
        0x25 => EntityAction(EntityAction) => crate::handler::play::handle_action,
        0x2c => IgnoredAdvancementTab(IgnoredAdvancementTab) => crate::handler::play::ignore,
        0x2f => HeldItemSlot(HeldItemSlot) => crate::handler::play::ignore,
        0x36 => ArmAnimation(ArmAnimation) => crate::handler::play::ignore,
        0x37 => IgnoredSpectate(IgnoredSpectate) => crate::handler::play::ignore,
        0x38 => IgnoredBlockPlace(IgnoredBlockPlace) => crate::handler::play::ignore,
        0x39 => IgnoredUseItem(IgnoredUseItem) => crate::handler::play::ignore,
    }
}

clientbound_packets! {
    Status {
        0x00 => Status(StatusResponse),
        0x01 => Pong(Pong),
    }
    Login {
        0x00 => LoginDisconnect(LoginDisconnect),
        0x01 => EncryptionRequest(EncryptionRequest),
        0x02 => LoginSuccess(LoginSuccess),
        0x03 => SetCompression(SetCompression),
    }
    Configuration {
        0x02 => ConfigurationDisconnect(ConfigurationDisconnect),
        0x03 => FinishConfiguration(FinishConfiguration),
        0x04 => ConfigurationKeepAlive(ConfigurationKeepAlive),
        0x09 => AddResourcePack(AddResourcePack),
        0x07 => RegistryData(RegistryData),
        0x0c => FeatureFlags(FeatureFlags),
        0x0d => UpdateTags(UpdateTags),
        0x0e => SelectKnownPacks(SelectKnownPacks),
    }
    Play {
        0x01 => SpawnPlayer(SpawnPlayer),
        0x0c => ChunkBatchFinished(ChunkBatchFinished),
        0x0d => ChunkBatchStart(ChunkBatchStart),
        0x1d => PlayDisconnect(PlayDisconnect),
        0x22 => GameEvent(GameEvent),
        0x25 => WorldBorder(WorldBorder),
        0x26 => KeepAlive(KeepAlive),
        0x27 => ChunkData(ChunkData),
        0x2b => JoinGame(JoinGame),
        0x30 => EntityLook(EntityLook),
        0x38 => PlayerAbilities(PlayerAbilities),
        0x3d => RemovePlayerInfo(RemovePlayerInfo),
        0x3e => PlayerInfo(PlayerInfo),
        0x40 => PlayerPosition(PlayerPosition),
        0x42 => RemoveEntities(RemoveEntities),
        0x48 => EntityHeadRotation(EntityHeadRotation),
        0x54 => CenterChunk(CenterChunk),
        0x56 => DefaultSpawnPosition(DefaultSpawnPosition),
        0x64 => TimeUpdate(TimeUpdate),
        0x69 => StartConfiguration(StartConfiguration),
        0x70 => EntityTeleport(EntityTeleport),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_id_in_different_states_is_valid() {
        assert_unique(&[(State::Handshake, 0), (State::Status, 0)]);
    }

    #[test]
    #[should_panic(expected = "duplicate packet ID")]
    fn duplicate_id_in_same_state_is_rejected() {
        assert_unique(&[(State::Status, 0), (State::Status, 0)]);
    }

    #[test]
    #[should_panic(expected = "packet ID must be non-negative")]
    fn negative_id_is_rejected() {
        assert_unique(&[(State::Status, -1)]);
    }
}
