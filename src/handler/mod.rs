//! Business handlers operate on typed packets, never on sockets or packet IDs.
pub(crate) mod handshake;
pub(crate) mod status;

use crate::protocol::{Clientbound, PROTOCOL_VERSION, Serverbound, State, invalid};
use std::io;

#[derive(Debug)]
pub struct Session {
    pub state: State,
    pub protocol: Option<i32>,
    pub address: Option<String>,
    pub port: Option<u16>,
    status_sent: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            state: State::Handshake,
            protocol: None,
            address: None,
            port: None,
            status_sent: false,
        }
    }
}

pub struct Outcome {
    pub reply: Option<Clientbound>,
    pub close: bool,
}

/// Only the services required by packet handlers; no transport access.
pub struct Context<'a> {
    pub session: &'a mut Session,
    pub status_json: &'a str,
}

pub struct Dispatcher {
    status_json: String,
}

impl Dispatcher {
    pub fn new(motd: &str, max_players: u32) -> Self {
        Self {
            status_json: serde_json::json!({
                "version": { "name": "1.21.1", "protocol": PROTOCOL_VERSION },
                "players": { "max": max_players, "online": 0, "sample": [] },
                "description": { "text": motd },
                "enforcesSecureChat": false
            })
            .to_string(),
        }
    }

    pub fn dispatch(&self, session: &mut Session, packet: Serverbound) -> io::Result<Outcome> {
        let outcome = packet.dispatch(&mut Context {
            session,
            status_json: &self.status_json,
        })?;
        // Handlers may transition state before replying (e.g. handshake -> login).
        if let Some(reply) = &outcome.reply
            && reply.state() != session.state
        {
            return Err(invalid("outbound packet not allowed in current session"));
        }
        Ok(outcome)
    }
}
