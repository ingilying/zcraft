//! Business handlers operate on typed packets, never on sockets or packet IDs.
pub(crate) mod handshake;
pub mod login;
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
        }
    }
}

pub struct Outcome {
    pub reply: Option<Clientbound>,
    pub close: bool,
    /// Enable the transport cipher before writing this outcome's reply.
    pub encryption: Option<[u8; 16]>,
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
        if let Some(reply) = &outcome.reply
            && reply.state() != ctx.session.state
        {
            return Err(invalid("outbound packet not allowed in current session"));
        }
        Ok(outcome)
    }
}
