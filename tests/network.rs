use std::{io::ErrorKind, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};
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

#[test]
fn state_packet_validation_and_dispatch() {
    assert!(decode(State::Handshake, &[1]).is_err());
    assert!(decode(State::Status, &[0, 1]).is_err());
    assert!(decode(State::Status, &[1, 0]).is_err());
    assert!(decode(State::Handshake, &handshake(4)).is_err());
    assert!(decode(State::Login, &[0]).is_err());
    let dispatcher = Dispatcher;
    let server = ServerContext {
        status: StatusService::new("test", 20),
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(
        listener,
        Config {
            io_timeout: deadline,
            ..Config::default()
        },
        async {
            let _ = rx.await;
        },
    ));
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
        write_frame(&mut stream, &handshake(2)).await.unwrap();
        let body = read_frame(&mut stream).await.unwrap().unwrap();
        let mut reader = Reader::new(&body);
        assert_eq!(reader.varint().unwrap(), 0);
        let json: serde_json::Value = serde_json::from_str(&reader.string(32767).unwrap()).unwrap();
        assert!(json["text"].as_str().unwrap().contains("not implemented"));
        assert!(read_frame(&mut stream).await.unwrap().is_none());
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
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
