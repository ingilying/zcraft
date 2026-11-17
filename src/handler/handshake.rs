use super::{Context, Outcome};
use crate::protocol::{
    PROTOCOL_VERSION, State, invalid,
    packets::{handshake::Handshake, login::LoginDisconnect},
};
use std::io;

pub fn handle_handshake(ctx: &mut Context<'_>, packet: Handshake) -> io::Result<Outcome> {
    if packet.next == State::Handshake {
        return Err(invalid("invalid state transition"));
    }
    ctx.session.protocol = Some(packet.protocol);
    ctx.session.address = Some(packet.address);
    ctx.session.port = Some(packet.port);
    ctx.session.state = packet.next;
    if packet.next == State::Login {
        let reason = if packet.protocol == PROTOCOL_VERSION {
            "ZCraft: login is not implemented yet."
        } else {
            "ZCraft: please use Minecraft Java 1.21.1."
        };
        return Ok(Outcome {
            reply: Some(
                LoginDisconnect {
                    reason: serde_json::json!({"text": reason}).to_string(),
                }
                .into(),
            ),
            close: true,
        });
    }
    Ok(Outcome {
        reply: None,
        close: false,
    })
}
