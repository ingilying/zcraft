pub mod icon;

use super::{Context, Outcome};
use crate::config::Config;
use crate::protocol::{
    Encode, PROTOCOL_VERSION, invalid,
    packets::status::{Ping, Pong, StatusRequest, StatusResponse},
};
use icon::ServerIcon;
use std::io;

/// Owns status response construction and caching. Currently all fields are static.
pub struct StatusService {
    response_json: String,
}

impl StatusService {
    pub fn from_config(config: &Config) -> io::Result<Self> {
        let icon = if config.server_icon.is_empty() {
            None
        } else {
            ServerIcon::load_optional(&config.server_icon)?
        };
        let service = Self::with_icon(&config.motd, config.max_players, icon.as_ref());
        // Reject an oversized combined response at startup, not on the first query.
        service.response().encode(&mut Vec::new())?;
        Ok(service)
    }

    pub fn new(motd: &str, max_players: u32) -> Self {
        Self::with_icon(motd, max_players, None)
    }

    pub fn with_icon(motd: &str, max_players: u32, icon: Option<&ServerIcon>) -> Self {
        let mut response = serde_json::json!({
            "version": { "name": "1.21.1", "protocol": PROTOCOL_VERSION },
            "players": { "max": max_players, "online": 0, "sample": [] },
            "description": { "text": motd },
            "enforcesSecureChat": false
        });
        if let Some(icon) = icon {
            response["favicon"] = icon.data_uri().into();
        }
        Self {
            response_json: response.to_string(),
        }
    }

    pub fn response(&self) -> StatusResponse {
        StatusResponse {
            json: self.response_json.clone(),
        }
    }
}

pub async fn handle_status_request(
    ctx: &mut Context<'_>,
    _packet: StatusRequest,
) -> io::Result<Outcome> {
    if ctx.session.status_sent {
        return Err(invalid("duplicate status request"));
    }
    ctx.session.status_sent = true;
    Ok(Outcome {
        reply: Some(ctx.server.status.response().into()),
        close: false,
        encryption: None,
    })
}

pub async fn handle_ping(_ctx: &mut Context<'_>, packet: Ping) -> io::Result<Outcome> {
    Ok(Outcome {
        reply: Some(
            Pong {
                payload: packet.payload,
            }
            .into(),
        ),
        close: true,
        encryption: None,
    })
}
