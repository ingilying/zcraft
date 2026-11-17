use super::{Context, Outcome};
use crate::protocol::{
    PROTOCOL_VERSION, invalid,
    packets::status::{Ping, Pong, StatusRequest, StatusResponse},
};
use std::io;

/// Owns status response construction and caching. Currently all fields are static.
pub struct StatusService {
    response_json: String,
}

impl StatusService {
    pub fn new(motd: &str, max_players: u32) -> Self {
        Self {
            response_json: serde_json::json!({
                "version": { "name": "1.21.1", "protocol": PROTOCOL_VERSION },
                "players": { "max": max_players, "online": 0, "sample": [] },
                "description": { "text": motd },
                "enforcesSecureChat": false
            })
            .to_string(),
        }
    }

    pub fn response(&self) -> StatusResponse {
        StatusResponse {
            json: self.response_json.clone(),
        }
    }
}

pub fn handle_status_request(ctx: &mut Context<'_>, _packet: StatusRequest) -> io::Result<Outcome> {
    if ctx.session.status_sent {
        return Err(invalid("duplicate status request"));
    }
    ctx.session.status_sent = true;
    Ok(Outcome {
        reply: Some(ctx.server.status.response().into()),
        close: false,
    })
}

pub fn handle_ping(_ctx: &mut Context<'_>, packet: Ping) -> io::Result<Outcome> {
    Ok(Outcome {
        reply: Some(
            Pong {
                payload: packet.payload,
            }
            .into(),
        ),
        close: true,
    })
}
