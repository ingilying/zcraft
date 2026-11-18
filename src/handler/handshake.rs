use super::{Context, Outcome, login::disconnect};
use crate::protocol::{PROTOCOL_VERSION, State, invalid, packets::handshake::Handshake};
use std::io;

pub async fn handle_handshake(ctx: &mut Context<'_>, packet: Handshake) -> io::Result<Outcome> {
    if !matches!(packet.next, State::Status | State::Login) {
        return Err(invalid("invalid state transition"));
    }
    ctx.session.protocol = Some(packet.protocol);
    ctx.session.address = Some(packet.address);
    ctx.session.port = Some(packet.port);
    ctx.session.state = packet.next;
    if packet.next == State::Login && packet.protocol != PROTOCOL_VERSION {
        return Ok(disconnect("ZCraft: please use Minecraft Java 1.21.1."));
    }
    Ok(Outcome {
        reply: None,
        close: false,
        encryption: None,
    })
}
