use zcraft::{
    handler::{Dispatcher, Session},
    protocol::{
        Clientbound, Reader, Serverbound, State, decode,
        packets::{
            handshake::Handshake,
            login::LoginDisconnect,
            status::{Ping, Pong, StatusRequest, StatusResponse},
        },
    },
};

#[test]
fn typed_inbound_conversion_and_generated_routing() {
    let dispatcher = Dispatcher::new("test", 20);
    let mut session = Session::default();
    let handshake: Serverbound = Handshake {
        protocol: 767,
        address: "localhost".into(),
        port: 25565,
        next: State::Status,
    }
    .into();
    assert_eq!(handshake.state(), State::Handshake);
    dispatcher.dispatch(&mut session, handshake).unwrap();
    let request: Serverbound = StatusRequest.into();
    assert_eq!(request.state(), State::Status);
    let status = dispatcher.dispatch(&mut session, request).unwrap();
    assert!(matches!(status.reply, Some(Clientbound::Status(_))));
    let ping: Serverbound = Ping { payload: i64::MIN }.into();
    let pong = dispatcher.dispatch(&mut session, ping).unwrap();
    assert_eq!(pong.reply, Some(Pong { payload: i64::MIN }.into()));
    assert!(pong.close);
}

#[test]
fn direct_packet_construction_cannot_bypass_state_guard() {
    let mut session = Session::default();
    let dispatcher = Dispatcher::new("test", 20);
    assert!(
        dispatcher
            .dispatch(&mut session, Ping { payload: 1 }.into())
            .is_err()
    );
    assert_eq!(session.state, State::Handshake);
}

#[test]
fn generated_decoder_validates_all_fields_and_trailing_bytes() {
    let mut bytes = vec![1];
    bytes.extend_from_slice(&42i64.to_be_bytes());
    assert_eq!(
        decode(State::Status, &bytes).unwrap(),
        Ping { payload: 42 }.into()
    );
    bytes.push(0);
    assert!(decode(State::Status, &bytes).is_err());
    assert!(decode(State::Status, &[0, 0]).is_err());
    assert!(decode(State::Status, &[2]).is_err());
}

#[test]
fn outbound_registry_owns_ids_and_states() {
    let status: Clientbound = StatusResponse { json: "{}".into() }.into();
    let disconnect: Clientbound = LoginDisconnect {
        reason: "{}".into(),
    }
    .into();
    assert_eq!(status.state(), State::Status);
    assert_eq!(disconnect.state(), State::Login);
    // Same ID across different states is intentional.
    for packet in [status, disconnect] {
        let bytes = packet.encode().unwrap();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.varint().unwrap(), 0);
        assert_eq!(reader.string(32767).unwrap(), "{}");
        reader.finish().unwrap();
    }
    let pong: Clientbound = Pong { payload: -42 }.into();
    assert_eq!(pong.state(), State::Status);
    let bytes = pong.encode().unwrap();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.varint().unwrap(), 1);
    assert_eq!(reader.i64().unwrap(), -42);
    reader.finish().unwrap();
}
