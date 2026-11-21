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
    pub(crate) entity_id: Option<i32>,
    chat_rate: play::chat::ChatRate,
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
            entity_id: None,
            chat_rate: play::chat::ChatRate::default(),
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
        let outcome = packet.dispatch(ctx).await?;
        // Handlers may transition state before replying (e.g. handshake -> login).
        for reply in &outcome.replies {
            if reply.state() != ctx.session.state {
                return Err(invalid("outbound packet not allowed in current session"));
            }
        }
        Ok(outcome)
    }
}
