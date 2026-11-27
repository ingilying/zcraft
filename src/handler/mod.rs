//! Business handlers operate on typed packets, never on sockets or packet IDs.
pub mod configuration;
pub(crate) mod handshake;
pub mod login;
pub mod play;
pub mod status;

pub use crate::context::Context;
use crate::protocol::{Clientbound, Serverbound, State, invalid};
use std::io;

#[derive(Debug)]
pub struct Session {
    pub state: State,
    pub protocol: Option<i32>,
    pub address: Option<String>,
    pub port: Option<u16>,
    status_sent: bool,
    pub login: login::LoginPhase,
    pub profile: Option<crate::identity::Profile>,
    pub(crate) reservation: Option<crate::players::Reservation>,
    pub configuration: configuration::ConfigurationPhase,
    pub(crate) configuration_runtime: Option<configuration::ConfigurationSession>,
    pub(crate) loaded_resource_pack: Option<(uuid::Uuid, String)>,
    pub client_information: Option<crate::protocol::packets::common::ClientInformation>,
    pub play: Option<play::PlaySession>,
    /// Authoritative player inventory survives Play reconfiguration on this connection.
    pub(crate) inventory: crate::game::Inventory,
    pub(crate) entity_id: Option<i32>,
    chat_rate: play::chat::ChatRate,
    next_completion: Option<tokio::time::Instant>,
}

impl Session {
    /// Call after successfully writing the entire Play initialization batch.
    /// Idempotent, including when the client returns from reconfiguration.
    pub fn play_initialized(&self) {
        if self.state == State::Play
            && self.configuration == configuration::ConfigurationPhase::Finished
            && self.play.is_some()
            && let Some(reservation) = &self.reservation
        {
            reservation.enter_play();
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            state: State::Handshake,
            protocol: None,
            address: None,
            port: None,
            status_sent: false,
            login: login::LoginPhase::default(),
            profile: None,
            reservation: None,
            configuration: configuration::ConfigurationPhase::NotStarted,
            configuration_runtime: None,
            loaded_resource_pack: None,
            client_information: None,
            play: None,
            inventory: crate::game::Inventory::default(),
            entity_id: None,
            chat_rate: play::chat::ChatRate::default(),
            next_completion: None,
        }
    }
}

pub struct Outcome {
    pub replies: Vec<Clientbound>,
    pub close: bool,
    /// Enable the transport cipher before writing this outcome's reply.
    pub encryption: Option<[u8; 16]>,
}

impl Outcome {
    pub fn empty() -> Self {
        Self {
            replies: Vec::new(),
            close: false,
            encryption: None,
        }
    }
}

/// Stateless routing and protocol-state validation; owns no business services.
pub struct Dispatcher;

impl Dispatcher {
    pub async fn dispatch(
        &self,
        ctx: &mut Context<'_>,
        packet: Serverbound,
    ) -> io::Result<Outcome> {
        // Also guards packets constructed directly rather than decoded from TCP.
        if ctx.session.state != packet.state() {
            return Err(invalid("packet not allowed in current session"));
        }
        let outcome = match packet {
            Serverbound::Handshake(p) => handshake::handle_handshake(ctx, p).await?,
            Serverbound::StatusRequest(p) => status::handle_status_request(ctx, p).await?,
            Serverbound::Ping(p) => status::handle_ping(ctx, p).await?,
            Serverbound::LoginStart(p) => login::handle_start(ctx, p).await?,
            Serverbound::EncryptionResponse(p) => login::handle_encryption_response(ctx, p).await?,
            Serverbound::LoginAcknowledged(p) => login::handle_acknowledged(ctx, p).await?,
            Serverbound::ConfigurationClientInformation(p) => {
                configuration::handle_information(ctx, p).await?
            }
            Serverbound::ConfigurationPluginMessage(p) => {
                configuration::handle_plugin(ctx, p).await?
            }
            Serverbound::FinishConfigurationAcknowledged(p) => {
                configuration::handle_finish(ctx, p).await?
            }
            Serverbound::ConfigurationKeepAliveResponse(p) => {
                configuration::handle_keep_alive(ctx, p).await?
            }
            Serverbound::ResourcePackResponse(p) => {
                configuration::handle_resource_pack(ctx, p).await?
            }
            Serverbound::KnownPacksResponse(p) => configuration::handle_known_packs(ctx, p).await?,
            Serverbound::TeleportConfirm(p) => play::handle_teleport(ctx, p).await?,
            Serverbound::ChatAcknowledgement(p) => play::ignore(ctx, p).await?,
            Serverbound::ChatCommand(p) => play::chat::handle_command(ctx, p).await?,
            Serverbound::SignedChatCommand(p) => play::chat::handle_signed_command(ctx, p).await?,
            Serverbound::ChatMessage(p) => play::chat::handle_message(ctx, p).await?,
            Serverbound::ChatSessionUpdate(p) => play::ignore(ctx, p).await?,
            Serverbound::ChunkBatchReceived(p) => play::handle_batch(ctx, p).await?,
            Serverbound::ClientCommand(p) => play::handle_client_command(ctx, p).await?,
            Serverbound::PlayClientInformation(p) => play::handle_information(ctx, p).await?,
            Serverbound::CommandCompletionRequest(p) => {
                play::chat::handle_completion(ctx, p).await?
            }
            Serverbound::ConfigurationAcknowledged(p) => {
                configuration::handle_reconfiguration_acknowledged(ctx, p).await?
            }
            Serverbound::InventoryClickPacket(p) => {
                play::blocks::handle_inventory_click(ctx, p).await?
            }
            Serverbound::CloseWindow(p) => play::blocks::handle_close_window(ctx, p).await?,
            Serverbound::PlayPluginMessage(p) => play::ignore(ctx, p).await?,
            Serverbound::KeepAliveResponse(p) => play::handle_keep_alive(ctx, p).await?,
            Serverbound::Position(p) => play::handle_position(ctx, p).await?,
            Serverbound::PositionLook(p) => play::handle_position_look(ctx, p).await?,
            Serverbound::Look(p) => play::handle_look(ctx, p).await?,
            Serverbound::Flying(p) => play::handle_flying(ctx, p).await?,
            Serverbound::Abilities(p) => play::ignore(ctx, p).await?,
            Serverbound::PlayerDigging(p) => play::blocks::handle_dig(ctx, p).await?,
            Serverbound::EntityAction(p) => play::handle_action(ctx, p).await?,
            Serverbound::IgnoredAdvancementTab(p) => play::ignore(ctx, p).await?,
            Serverbound::HeldItemSlot(p) => play::blocks::handle_slot(ctx, p).await?,
            Serverbound::CreativeSlot(p) => play::blocks::handle_creative_slot(ctx, p).await?,
            Serverbound::ArmAnimation(p) => play::handle_arm_animation(ctx, p).await?,
            Serverbound::IgnoredSpectate(p) => play::ignore(ctx, p).await?,
            Serverbound::BlockPlace(p) => play::blocks::handle_place(ctx, p).await?,
            Serverbound::UseItem(p) => play::blocks::handle_use_item(ctx, p).await?,
        };
        // Handlers may transition state before replying (e.g. handshake -> login).
        for reply in &outcome.replies {
            if reply.state() != ctx.session.state {
                return Err(invalid("outbound packet not allowed in current session"));
            }
        }
        Ok(outcome)
    }
}
