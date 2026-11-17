//! Protocol 767 mappings. Each direction has its own ID namespace.
//! Macros generate static matches, not heap-allocated trait objects.
use super::{
    Decode, Encode, Reader, State, invalid,
    packets::{
        handshake::Handshake,
        login::LoginDisconnect,
        status::{Ping, Pong, StatusRequest, StatusResponse},
    },
    put_varint,
};
use crate::handler::{Context, Outcome};
use std::io;

const fn assert_unique(entries: &[(State, i32)]) {
    let mut i = 0;
    while i < entries.len() {
        assert!(entries[i].1 >= 0, "packet ID must be non-negative");
        let mut j = i + 1;
        while j < entries.len() {
            assert!(
                entries[i].0 as u8 != entries[j].0 as u8 || entries[i].1 != entries[j].1,
                "duplicate packet ID in the same state and direction"
            );
            j += 1;
        }
        i += 1;
    }
}

macro_rules! serverbound_packets {
    ($( $state:ident { $( $id:literal => $variant:ident($packet:ty) => $handler:path ),* $(,)? } )*) => {
        const _: () = assert_unique(&[$($( (State::$state, $id), )*)*]);

        #[derive(Debug, PartialEq, Eq)]
        pub enum Serverbound {
            $($( $variant($packet), )*)*
        }

        $($(
            impl From<$packet> for Serverbound {
                fn from(packet: $packet) -> Self { Self::$variant(packet) }
            }
        )*)*

        pub fn decode(state: State, body: &[u8]) -> io::Result<Serverbound> {
            let mut reader = Reader::new(body);
            let id = reader.varint()?;
            let packet = match (state, id) {
                $($( (State::$state, $id) => Serverbound::$variant(
                    <$packet as Decode>::decode(&mut reader)?
                ), )*)*
                _ => return Err(invalid("unknown packet ID for connection state")),
            };
            // Always validate before running any business logic.
            reader.finish()?;
            Ok(packet)
        }

        impl Serverbound {
            pub fn state(&self) -> State {
                match self {
                    $($( Self::$variant(_) => State::$state, )*)*
                }
            }

            pub(crate) fn dispatch(self, ctx: &mut Context<'_>) -> io::Result<Outcome> {
                // Also guards packets constructed directly rather than decoded from TCP.
                if ctx.session.state != self.state() {
                    return Err(invalid("packet not allowed in current session"));
                }
                match self {
                    $($( Self::$variant(packet) => $handler(ctx, packet), )*)*
                }
            }
        }
    };
}

macro_rules! clientbound_packets {
    ($( $state:ident { $( $id:literal => $variant:ident($packet:ty) ),* $(,)? } )*) => {
        const _: () = assert_unique(&[$($( (State::$state, $id), )*)*]);

        #[derive(Debug, PartialEq, Eq)]
        pub enum Clientbound {
            $($( $variant($packet), )*)*
        }

        $($(
            impl From<$packet> for Clientbound {
                fn from(packet: $packet) -> Self { Self::$variant(packet) }
            }
        )*)*

        impl Clientbound {
            pub fn state(&self) -> State {
                match self {
                    $($( Self::$variant(_) => State::$state, )*)*
                }
            }

            pub fn encode(&self) -> io::Result<Vec<u8>> {
                let mut out = Vec::new();
                match self {
                    $($( Self::$variant(packet) => {
                        put_varint($id, &mut out);
                        <$packet as Encode>::encode(packet, &mut out)?;
                    }, )*)*
                }
                Ok(out)
            }
        }
    };
}

// Add a packet here after defining its fields/codec and typed handler.
serverbound_packets! {
    Handshake {
        0x00 => Handshake(Handshake) => crate::handler::handshake::handle_handshake,
    }
    Status {
        0x00 => StatusRequest(StatusRequest) => crate::handler::status::handle_status_request,
        0x01 => Ping(Ping) => crate::handler::status::handle_ping,
    }
}

clientbound_packets! {
    Status {
        0x00 => Status(StatusResponse),
        0x01 => Pong(Pong),
    }
    Login {
        0x00 => LoginDisconnect(LoginDisconnect),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_id_in_different_states_is_valid() {
        assert_unique(&[(State::Handshake, 0), (State::Status, 0)]);
    }

    #[test]
    #[should_panic(expected = "duplicate packet ID")]
    fn duplicate_id_in_same_state_is_rejected() {
        assert_unique(&[(State::Status, 0), (State::Status, 0)]);
    }

    #[test]
    #[should_panic(expected = "packet ID must be non-negative")]
    fn negative_id_is_rejected() {
        assert_unique(&[(State::Status, -1)]);
    }
}
