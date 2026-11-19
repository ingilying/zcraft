pub mod icon;

use super::{Context, Outcome};
use crate::protocol::{
    Encode, PROTOCOL_VERSION, invalid,
    packets::status::{Ping, Pong, StatusRequest, StatusResponse},
};
use crate::{config::Config, players::Players};
use icon::ServerIcon;
use std::io;

/// Owns the status template and its authoritative player registry.
pub struct StatusService {
    template: serde_json::Value,
    players: Players,
}

impl StatusService {
    pub fn from_config(config: &Config) -> io::Result<Self> {
        let icon = if config.server_icon.is_empty() {
            None
        } else {
            ServerIcon::load_optional(&config.server_icon)?
        };
        let service = Self::with_icon(&config.motd, config.max_players, icon.as_ref());
        service.validate()?;
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
            template: response,
            players: Players::new(max_players),
        }
    }

    pub fn players(&self) -> &Players {
        &self.players
    }

    pub fn response(&self) -> StatusResponse {
        self.response_with_count(self.players.online_count())
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        // Reject oversized responses even when the maximum is reached.
        self.response_with_count(self.players.max_players())
            .encode(&mut Vec::new())
    }

    fn response_with_count(&self, online: u32) -> StatusResponse {
        let mut response = self.template.clone();
        response["players"]["online"] = online.into();
        StatusResponse {
            json: response.to_string(),
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
        replies: vec![ctx.server.status.response().into()],
        close: false,
        encryption: None,
    })
}

pub async fn handle_ping(_ctx: &mut Context<'_>, packet: Ping) -> io::Result<Outcome> {
    Ok(Outcome {
        replies: vec![
            Pong {
                payload: packet.payload,
            }
            .into(),
        ],
        close: true,
        encryption: None,
    })
}
