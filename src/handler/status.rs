use super::{Context, Outcome};
use crate::protocol::{
    invalid,
    packets::status::{Ping, Pong, StatusRequest, StatusResponse},
};
use std::io;

pub fn handle_status_request(ctx: &mut Context<'_>, _packet: StatusRequest) -> io::Result<Outcome> {
    if ctx.session.status_sent {
        return Err(invalid("duplicate status request"));
    }
    ctx.session.status_sent = true;
    Ok(Outcome {
        reply: Some(
            StatusResponse {
                json: ctx.status_json.to_owned(),
            }
            .into(),
        ),
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
