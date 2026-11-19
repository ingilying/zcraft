mod common;

use common::negotiate_compression;
use std::{io::ErrorKind, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};
use zcraft::transport::Transport;
use zcraft::{
    context::{Context, ServerContext},
    handler::{Dispatcher, Session, status::StatusService},
    network::{Config, serve},
    protocol::{
        packets::status::{Ping, StatusRequest},
        *,
    },
};

fn handshake(next: i32) -> Vec<u8> {
    let mut bytes = vec![0];
    put_varint(PROTOCOL_VERSION, &mut bytes);
    put_string("localhost", &mut bytes).unwrap();
    bytes.extend_from_slice(&25565u16.to_be_bytes());
    put_varint(next, &mut bytes);
    bytes
}

fn framed(body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    put_varint(body.len() as i32, &mut bytes);
    bytes.extend_from_slice(body);
    bytes
}

#[test]
fn varint_roundtrip_and_overflow() {
    for value in [0, 1, 127, 128, 255, 767, i32::MAX, -1, i32::MIN] {
        let mut bytes = Vec::new();
        put_varint(value, &mut bytes);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.varint().unwrap(), value);
        reader.finish().unwrap();
    }
    for bytes in [&[0x80][..], &[0xff, 0xff, 0xff, 0xff, 0x10], &[0x80; 6]] {
        assert!(Reader::new(bytes).varint().is_err());
    }
}

#[test]
fn strings_validate_lengths_utf8_and_utf16() {
    assert!(Reader::new(&[1, 0xff]).string(255).is_err());
    assert!(Reader::new(&[2, b'a']).string(255).is_err());
    assert!(
        Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x0f])
            .string(255)
            .is_err()
    );
    let mut bytes = Vec::new();
    put_string("😀", &mut bytes).unwrap();
    assert!(Reader::new(&bytes).string(1).is_err());
    assert_eq!(Reader::new(&bytes).string(2).unwrap(), "😀");
}

#[tokio::test]
async fn state_packet_validation_and_dispatch() {
    assert!(decode(State::Handshake, &[1]).is_err());
    assert!(decode(State::Status, &[0, 1]).is_err());
    assert!(decode(State::Status, &[1, 0]).is_err());
    assert!(decode(State::Handshake, &handshake(4)).is_err());
    assert!(decode(State::Login, &[0]).is_err());
    let dispatcher = Dispatcher;
    let server = ServerContext {
        status: StatusService::new("test", 20),
        login: zcraft::handler::login::LoginService::offline(),
        configuration: zcraft::handler::configuration::ConfigurationService::new().unwrap(),
        play: zcraft::handler::play::PlayService::new().unwrap(),
    };
    let mut session = Session::default();
    assert!(
        dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server
                },
                StatusRequest.into()
            )
            .await
            .is_err()
    );
    dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            decode(State::Handshake, &handshake(1)).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(session.state, State::Status);
    assert_eq!(session.protocol, Some(767));
    assert!(
        !dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server
                },
                StatusRequest.into()
            )
            .await
            .unwrap()
            .close
    );
    assert!(
        dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server
                },
                StatusRequest.into()
            )
            .await
            .is_err()
    );
    assert!(
        dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server
                },
                Ping { payload: -42 }.into()
            )
            .await
            .unwrap()
            .close
    );
}

#[tokio::test]
async fn fragmented_and_coalesced_frames() {
    let (mut writer, mut reader) = tokio::io::duplex(1);
    let task = tokio::spawn(async move {
        for byte in [3, 0, 1, 2, 1, 0] {
            writer.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    assert_eq!(read_frame(&mut reader).await.unwrap(), Some(vec![0, 1, 2]));
    assert_eq!(read_frame(&mut reader).await.unwrap(), Some(vec![0]));
    assert!(read_frame(&mut reader).await.unwrap().is_none());
    task.await.unwrap();
}

#[tokio::test]
async fn rejects_invalid_and_truncated_frames() {
    for bytes in [&[0][..], &[0x80, 0x80, 0x80], &[0x80], &[3, 0]] {
        assert!(read_frame(&mut &bytes[..]).await.is_err());
    }
    assert_eq!(
        read_frame(&mut &[3, 0][..]).await.unwrap_err().kind(),
        ErrorKind::UnexpectedEof
    );
    assert!(write_frame(&mut tokio::io::sink(), &[]).await.is_err());
    assert!(
        write_frame(&mut tokio::io::sink(), &vec![0; MAX_FRAME_SIZE + 1])
            .await
            .is_err()
    );
}

async fn start(
    deadline: Duration,
) -> (
    TcpStream,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    start_config(Config {
        io_timeout: deadline,
        offline_mode: true,
        server_icon: concat!(env!("CARGO_MANIFEST_DIR"), "/server-icon.png").into(),
        ..Config::default()
    })
    .await
}

async fn start_config(
    config: Config,
) -> (
    TcpStream,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(listener, config, async {
        let _ = rx.await;
    }));
    (TcpStream::connect(address).await.unwrap(), tx, task)
}

#[tokio::test]
async fn tcp_status_ping_and_shutdown() {
    timeout(Duration::from_secs(5), async {
        let (mut stream, stop, server) = start(Duration::from_secs(1)).await;
        let mut request = framed(&handshake(1));
        request.extend(framed(&[0]));
        let mut ping = vec![1];
        ping.extend_from_slice(&(-123456789i64).to_be_bytes());
        request.extend(framed(&ping));
        stream.write_all(&request).await.unwrap();
        let status = read_frame(&mut stream).await.unwrap().unwrap();
        let mut reader = Reader::new(&status);
        assert_eq!(reader.varint().unwrap(), 0);
        let json: serde_json::Value = serde_json::from_str(&reader.string(32767).unwrap()).unwrap();
        reader.finish().unwrap();
        assert_eq!(json["version"]["protocol"], 767);
        assert_eq!(json["players"]["online"], 0);
        let expected_icon = zcraft::handler::status::icon::ServerIcon::from_png(include_bytes!(
            "../server-icon.png"
        ))
        .unwrap();
        assert_eq!(json["favicon"], expected_icon.data_uri());
        assert_eq!(read_frame(&mut stream).await.unwrap().unwrap(), ping);
        assert!(read_frame(&mut stream).await.unwrap().is_none());
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn tcp_login_disconnect() {
    timeout(Duration::from_secs(5), async {
        let (mut stream, stop, server) = start(Duration::from_secs(1)).await;
        let mut wrong_version = handshake(2);
        // Replace the protocol VarInt (767 = ff 05) with 766 = fe 05.
        wrong_version[1] = 0xfe;
        write_frame(&mut stream, &wrong_version).await.unwrap();
        let body = read_frame(&mut stream).await.unwrap().unwrap();
        let mut reader = Reader::new(&body);
        assert_eq!(reader.varint().unwrap(), 0);
        let json: serde_json::Value = serde_json::from_str(&reader.string(32767).unwrap()).unwrap();
        assert!(json["text"].as_str().unwrap().contains("1.21.1"));
        assert!(read_frame(&mut stream).await.unwrap().is_none());
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

async fn enter_configuration(stream: &mut Transport<&mut TcpStream>) {
    stream.write_frame(&handshake(2)).await.unwrap();
    let mut login = vec![0];
    put_string("Steve", &mut login).unwrap();
    login.extend_from_slice(&[0; 16]);
    stream.write_frame(&login).await.unwrap();
    negotiate_compression(stream).await;
    assert_eq!(stream.read_frame().await.unwrap().unwrap()[0], 2);
    stream.write_frame(&[3]).await.unwrap();
    assert_eq!(stream.read_frame().await.unwrap().unwrap()[0], 0x0c);
    assert_eq!(stream.read_frame().await.unwrap().unwrap(), [0x0e, 0]);
}

fn assert_configuration_disconnect(body: &[u8], message: &str) {
    assert_eq!(&body[..2], &[2, 8]); // Configuration Disconnect, anonymous NBT string
    assert!(
        body.windows(message.len())
            .any(|window| window == message.as_bytes())
    );
}

#[tokio::test]
async fn tcp_configuration_decode_and_dispatch_errors_send_disconnect() {
    timeout(Duration::from_secs(10), async {
        for bad_packet in [
            vec![0xff],
            vec![3],
            vec![7, 1],
            vec![4, 0, 0, 0, 0, 0, 0, 0, 0],
        ] {
            let (mut stream, stop, server) = start(Duration::from_secs(1)).await;
            let mut client = Transport::new(&mut stream);
            enter_configuration(&mut client).await;
            client.write_frame(&bad_packet).await.unwrap();
            let body = client.read_frame().await.unwrap().unwrap();
            assert_configuration_disconnect(&body, "invalid configuration packet");
            assert!(client.read_frame().await.unwrap().is_none());
            stop.send(()).unwrap();
            server.await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn configuration_idle_uses_phase_deadline_but_partial_frames_still_time_out() {
    timeout(Duration::from_secs(5), async {
        let (mut stream, stop, server) = start(Duration::from_millis(150)).await;
        let mut client = Transport::new(&mut stream);
        enter_configuration(&mut client).await;
        // An idle configuration prompt is allowed to outlive the frame timeout.
        assert!(
            timeout(Duration::from_millis(300), client.read_frame())
                .await
                .is_err()
        );
        drop(client); // No partial inbound packet at this point.
        stream.write_all(&[0x80]).await.unwrap();
        let mut client = Transport::new(&mut stream);
        client
            .enable_compression(zcraft::config::DEFAULT_COMPRESSION_THRESHOLD as usize)
            .unwrap();
        let body = client.read_frame().await.unwrap().unwrap();
        assert_configuration_disconnect(&body, "configuration timed out");
        assert!(client.read_frame().await.unwrap().is_none());
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn configured_resource_pack_is_negotiated_on_the_wire() {
    timeout(Duration::from_secs(5), async {
        for required in [false, true] {
            let id = uuid::Uuid::from_bytes([1; 16]);
            let (mut stream, stop, server) = start_config(Config {
                io_timeout: Duration::from_secs(1),
                offline_mode: true,
                server_icon: concat!(env!("CARGO_MANIFEST_DIR"), "/server-icon.png").into(),
                resource_pack: Some(zcraft::config::ResourcePackConfig {
                    id: id.to_string(),
                    url: "https://example.org/pack.zip".into(),
                    sha1: "0".repeat(40),
                    required,
                    prompt: None,
                }),
                ..Default::default()
            })
            .await;
            let mut client = Transport::new(&mut stream);
            enter_configuration(&mut client).await;
            client.write_frame(&[7, 0]).await.unwrap();
            for _ in 0..11 {
                assert_eq!(client.read_frame().await.unwrap().unwrap()[0], 7);
            }
            assert_eq!(client.read_frame().await.unwrap().unwrap()[0], 0x0d);
            let offer = client.read_frame().await.unwrap().unwrap();
            let mut reader = Reader::new(&offer);
            assert_eq!(reader.varint().unwrap(), 9);
            assert_eq!(reader.take(16).unwrap(), id.as_bytes());
            assert_eq!(
                reader.string(32767).unwrap(),
                "https://example.org/pack.zip"
            );
            assert_eq!(reader.string(40).unwrap(), "0".repeat(40));
            assert_eq!(reader.take(2).unwrap(), [u8::from(required), 0]);
            reader.finish().unwrap();
            let mut reply = vec![6];
            reply.extend_from_slice(id.as_bytes());
            reply.push(1); // decline
            client.write_frame(&reply).await.unwrap();
            let body = client.read_frame().await.unwrap().unwrap();
            if required {
                assert_configuration_disconnect(&body, "required resource pack");
                assert!(client.read_frame().await.unwrap().is_none());
            } else {
                assert_eq!(body, [3]);
            }
            stop.send(()).unwrap();
            server.await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn partial_frame_times_out_and_shutdown_closes_idle_socket() {
    let (mut stream, stop, server) = start(Duration::from_millis(50)).await;
    stream.write_all(&[0x80]).await.unwrap();
    let mut byte = [0];
    assert_eq!(
        timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();

    let (mut stream, stop, server) = start(Duration::from_secs(60)).await;
    stop.send(()).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // A socket not yet accepted may be reset rather than receive a clean FIN.
    let result = timeout(Duration::from_secs(2), stream.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
}
