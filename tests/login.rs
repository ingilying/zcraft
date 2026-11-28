mod common;

use common::negotiate_compression;
use openssl::{encrypt::Encrypter, pkey::PKey, rsa::Padding};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;
use zcraft::{
    auth::{AuthService, SessionVerifier, offline_profile},
    config::Config,
    context::ServerContext,
    handler::{login::LoginService, status::StatusService},
    identity::{Profile, ProfileProperty},
    network::serve_with_context,
    protocol::{Reader, State, decode, put_string, put_varint, read_frame, write_frame},
    transport::Transport,
};

struct Verifier {
    calls: AtomicUsize,
    allow: bool,
}
impl SessionVerifier for Verifier {
    fn verify<'a>(
        &'a self,
        name: &'a str,
        hash: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Profile>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(name, "Notch");
        assert!(!hash.is_empty());
        Box::pin(async move {
            if !self.allow {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
            }
            Ok(online_profile())
        })
    }
}
fn online_profile() -> Profile {
    Profile {
        uuid: Uuid::parse_str("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap(),
        name: "Notch".into(),
        properties: vec![ProfileProperty {
            name: "textures".into(),
            value: "texture".into(),
            signature: Some("signature".into()),
        }],
    }
}

async fn start(
    login: LoginService,
) -> (TcpStream, oneshot::Sender<()>, JoinHandle<io::Result<()>>) {
    start_with_threshold(login, zcraft::config::DEFAULT_COMPRESSION_THRESHOLD).await
}

async fn start_with_threshold(
    login: LoginService,
    compression_threshold: i32,
) -> (TcpStream, oneshot::Sender<()>, JoinHandle<io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Arc::new(ServerContext {
        status: StatusService::new("test", 20),
        login,
        configuration: zcraft::handler::configuration::ConfigurationService::new(),
        play: zcraft::handler::play::PlayService::new().unwrap(),
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(serve_with_context(
        listener,
        Config {
            compression_threshold,
            ..Config::default()
        },
        server,
        async {
            let _ = stopped.await;
        },
    ));
    (TcpStream::connect(address).await.unwrap(), stop, task)
}
fn handshake() -> Vec<u8> {
    let mut bytes = vec![0];
    put_varint(767, &mut bytes);
    put_string("localhost", &mut bytes).unwrap();
    bytes.extend_from_slice(&25565u16.to_be_bytes());
    put_varint(2, &mut bytes);
    bytes
}
fn login_start(name: &str) -> Vec<u8> {
    let mut bytes = vec![0];
    put_string(name, &mut bytes).unwrap();
    bytes.extend_from_slice(Uuid::nil().as_bytes()); // Must not become the accepted UUID.
    bytes
}
fn assert_success(body: &[u8], expected: &Profile) {
    let mut reader = Reader::new(body);
    assert_eq!(reader.varint().unwrap(), 2);
    assert_eq!(reader.take(16).unwrap(), expected.uuid.as_bytes());
    assert_eq!(reader.string(16).unwrap(), expected.name);
    assert_eq!(reader.varint().unwrap() as usize, expected.properties.len());
    for property in &expected.properties {
        assert_eq!(reader.string(32767).unwrap(), property.name);
        assert_eq!(reader.string(32767).unwrap(), property.value);
        assert_eq!(
            reader.take(1).unwrap(),
            &[u8::from(property.signature.is_some())]
        );
        if let Some(signature) = &property.signature {
            assert_eq!(&reader.string(32767).unwrap(), signature);
        }
    }
    assert_eq!(reader.take(1).unwrap(), &[1]); // strictErrorHandling
    reader.finish().unwrap();
}
async fn configuration_end(client: &mut Transport<TcpStream>) {
    client.write_frame(&[3]).await.unwrap(); // Login Acknowledged
    let flags = client.read_frame().await.unwrap().unwrap();
    let mut reader = Reader::new(&flags);
    assert_eq!(reader.varint().unwrap(), 0x0c);
    assert_eq!(reader.varint().unwrap(), 1);
    assert_eq!(reader.string(32767).unwrap(), "minecraft:vanilla");
    reader.finish().unwrap();
    assert_eq!(client.read_frame().await.unwrap().unwrap(), [0x0e, 0]);

    let mut information = vec![0]; // Configuration Client Information
    put_string("en_us", &mut information).unwrap();
    information.extend_from_slice(&[8, 0, 1, 0x7f, 1, 0, 1]);
    client.write_frame(&information).await.unwrap();
    client.write_frame(&[7, 0]).await.unwrap(); // Selected Known Packs: empty

    let expected = zcraft::registry_data::registry_packets();
    for registry in expected {
        let frame = client.read_frame().await.unwrap().unwrap();
        assert_eq!(frame[0], 7);
        assert_eq!(&frame[1..], registry);
    }
    let mut expected_tags = vec![0x0d];
    expected_tags.extend(zcraft::registry_data::tags_packet());
    assert_eq!(client.read_frame().await.unwrap().unwrap(), expected_tags);
    assert_eq!(client.read_frame().await.unwrap().unwrap(), [3]); // Finish Configuration
    client.write_frame(&[3]).await.unwrap(); // Acknowledge Finish

    let expected_ids = [
        0x2b, 0x0b, 0x3e, 0x38, 0x56, 0x64, 0x25, 0x54, 0x22, 0x11, 0x5d, 0x13, 0x53, 0x0d,
    ];
    let mut entity = 0;
    for id in expected_ids {
        let frame = client.read_frame().await.unwrap().unwrap();
        assert_eq!(frame[0], id);
        if id == 0x2b {
            entity = i32::from_be_bytes(frame[1..5].try_into().unwrap());
            assert!(entity > 0);
        }
    }
    let mut chunks = std::collections::BTreeSet::new();
    for index in 0..9 {
        let frame = client.read_frame().await.unwrap().unwrap();
        assert_eq!(frame[0], 0x27);
        let x = i32::from_be_bytes(frame[1..5].try_into().unwrap());
        let z = i32::from_be_bytes(frame[5..9].try_into().unwrap());
        if index == 0 {
            assert_eq!((x, z), (0, 0));
        }
        assert!(chunks.insert((x, z)));
        assert_eq!(&frame[1..], zcraft::world::chunk_data(x, z).unwrap());
    }
    assert_eq!(client.read_frame().await.unwrap().unwrap(), [0x0c, 9]);
    let position = client.read_frame().await.unwrap().unwrap();
    assert_eq!(position[0], 0x40);
    let mut reader = Reader::new(&position[1..]);
    reader.take(8 * 3 + 4 * 2 + 1).unwrap();
    assert_eq!(reader.varint().unwrap(), 0);
    reader.finish().unwrap();
    client.write_frame(&[0, 0]).await.unwrap(); // teleport confirm
    let mut batch = vec![8];
    batch.extend_from_slice(&20f32.to_be_bytes());
    client.write_frame(&batch).await.unwrap();
    // Test basic spectator movement followed by an out-of-bounds correction.
    let mut position = vec![0x1a];
    for coordinate in [9.0f64, 66.0, 9.0] {
        position.extend_from_slice(&coordinate.to_be_bytes());
    }
    position.push(0);
    client.write_frame(&position).await.unwrap();
    let mut outside = vec![0x1a];
    for coordinate in [30_000_000.0f64, 66.0, 9.0] {
        outside.extend_from_slice(&coordinate.to_be_bytes());
    }
    outside.push(0);
    client.write_frame(&outside).await.unwrap();
    // Chunk streaming (0x0d/0x27/0x0c) and periodic TimeUpdate (0x64) can
    // arrive before the next 20 TPS game tick emits the teleport correction.
    let correction = loop {
        let frame = client.read_frame().await.unwrap().unwrap();
        match frame[0] {
            0x0c | 0x0d | 0x27 | 0x64 => continue,
            _ => break frame,
        }
    };
    assert_eq!(correction[0], 0x40);
    assert_eq!(*correction.last().unwrap(), 1);
    client.write_frame(&[0, 1]).await.unwrap();
    // Valid spectator action refers to this connection's assigned entity.
    let mut action = vec![0x25];
    put_varint(entity, &mut action);
    action.extend([0, 0]);
    client.write_frame(&action).await.unwrap();
}
async fn stop_server(stop: oneshot::Sender<()>, task: JoinHandle<io::Result<()>>) {
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn offline_login_ignores_claimed_uuid_then_enters_configuration() {
    timeout(Duration::from_secs(5), async {
        let (stream, stop, task) = start(LoginService::offline()).await;
        let mut client = Transport::new(stream);
        client.write_frame(&handshake()).await.unwrap();
        client.write_frame(&login_start("Steve")).await.unwrap();
        negotiate_compression(&mut client).await;
        assert_success(
            &client.read_frame().await.unwrap().unwrap(),
            &offline_profile("Steve"),
        );
        configuration_end(&mut client).await;
        stop_server(stop, task).await;
    })
    .await
    .unwrap();
}

// Perform the wire RSA exchange with a fragmented plaintext response. The next
// server packet must already be encrypted, including its length prefix.
async fn exchange(mut stream: TcpStream, wrong_token: bool) -> Transport<TcpStream> {
    write_frame(&mut stream, &handshake()).await.unwrap();
    write_frame(&mut stream, &login_start("Notch"))
        .await
        .unwrap();
    let body = read_frame(&mut stream).await.unwrap().unwrap();
    let mut reader = Reader::new(&body);
    assert_eq!(reader.varint().unwrap(), 1);
    assert_eq!(reader.string(20).unwrap(), "");
    let key = reader.byte_array(1024).unwrap();
    let mut token = reader.byte_array(4).unwrap();
    assert_eq!(reader.take(1).unwrap(), &[1]); // shouldAuthenticate
    reader.finish().unwrap();
    if wrong_token {
        token[0] ^= 1;
    }
    let key = PKey::public_key_from_der(&key).unwrap();
    let encrypt = |plain: &[u8]| {
        let mut enc = Encrypter::new(&key).unwrap();
        enc.set_rsa_padding(Padding::PKCS1).unwrap();
        let mut out = vec![0; enc.encrypt_len(plain).unwrap()];
        let len = enc.encrypt(plain, &mut out).unwrap();
        out.truncate(len);
        out
    };
    let secret = [0x31; 16];
    let mut body = vec![1];
    for plain in [&secret[..], &token[..]] {
        let encrypted = encrypt(plain);
        put_varint(encrypted.len() as i32, &mut body);
        body.extend(encrypted);
    }
    let mut frame = Vec::new();
    put_varint(body.len() as i32, &mut frame);
    frame.extend(body);
    for chunk in frame.chunks(7) {
        stream.write_all(chunk).await.unwrap();
    }
    let mut client = Transport::new(stream);
    client.enable_encryption(&secret).unwrap();
    client
}

#[tokio::test]
async fn online_login_verifies_identity_and_encrypts_both_directions() {
    timeout(Duration::from_secs(10), async {
        let verifier = Arc::new(Verifier {
            calls: AtomicUsize::new(0),
            allow: true,
        });
        let auth = Arc::new(AuthService::with_verifier(verifier.clone()).unwrap());
        let (stream, stop, task) = start(LoginService::online(auth)).await;
        let mut client = exchange(stream, false).await;
        negotiate_compression(&mut client).await;
        assert_success(
            &client.read_frame().await.unwrap().unwrap(),
            &online_profile(),
        );
        configuration_end(&mut client).await;
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
        stop_server(stop, task).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_verification_sends_encrypted_disconnect_without_offline_fallback() {
    timeout(Duration::from_secs(10), async {
        let verifier = Arc::new(Verifier {
            calls: AtomicUsize::new(0),
            allow: false,
        });
        let auth = Arc::new(AuthService::with_verifier(verifier.clone()).unwrap());
        let (stream, stop, task) = start(LoginService::online(auth)).await;
        let mut client = exchange(stream, false).await;
        let body = client.read_frame().await.unwrap().unwrap();
        let mut reader = Reader::new(&body);
        assert_eq!(reader.varint().unwrap(), 0);
        assert!(reader.string(32767).unwrap().contains("failed to verify"));
        reader.finish().unwrap();
        assert!(client.read_frame().await.unwrap().is_none());
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
        stop_server(stop, task).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_challenge_closes_connection_without_calling_mojang() {
    timeout(Duration::from_secs(10), async {
        let verifier = Arc::new(Verifier {
            calls: AtomicUsize::new(0),
            allow: true,
        });
        let auth = Arc::new(AuthService::with_verifier(verifier.clone()).unwrap());
        let (stream, stop, task) = start(LoginService::online(auth)).await;
        let mut client = exchange(stream, true).await;
        assert!(client.read_frame().await.unwrap().is_none());
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
        stop_server(stop, task).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_names_and_out_of_order_packets_do_not_log_in() {
    timeout(Duration::from_secs(5), async {
        for name in ["", "bad name", "世界"] {
            let (stream, stop, task) = start(LoginService::offline()).await;
            let mut client = Transport::new(stream);
            client.write_frame(&handshake()).await.unwrap();
            client.write_frame(&login_start(name)).await.unwrap();
            let body = client.read_frame().await.unwrap().unwrap();
            let mut reader = Reader::new(&body);
            assert_eq!(reader.varint().unwrap(), 0);
            assert!(reader.string(32767).unwrap().contains("invalid username"));
            assert!(client.read_frame().await.unwrap().is_none());
            stop_server(stop, task).await;
        }
        for body in [vec![3], vec![1, 0, 0]] {
            let (stream, stop, task) = start(LoginService::offline()).await;
            let mut client = Transport::new(stream);
            client.write_frame(&handshake()).await.unwrap();
            client.write_frame(&body).await.unwrap();
            assert!(client.read_frame().await.unwrap().is_none());
            stop_server(stop, task).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn duplicate_login_start_is_rejected() {
    timeout(Duration::from_secs(5), async {
        let (stream, stop, task) = start(LoginService::offline()).await;
        let mut client = Transport::new(stream);
        client.write_frame(&handshake()).await.unwrap();
        client.write_frame(&login_start("Steve")).await.unwrap();
        negotiate_compression(&mut client).await;
        assert_success(
            &client.read_frame().await.unwrap().unwrap(),
            &offline_profile("Steve"),
        );
        client.write_frame(&login_start("Alex")).await.unwrap();
        assert!(client.read_frame().await.unwrap().is_none());
        stop_server(stop, task).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn compression_switch_uses_old_framing_and_zero_compresses_login_success() {
    timeout(Duration::from_secs(10), async {
        for threshold in [-1, 0, 256] {
            let (mut stream, stop, task) =
                start_with_threshold(LoginService::offline(), threshold).await;
            write_frame(&mut stream, &handshake()).await.unwrap();
            write_frame(&mut stream, &login_start("Steve"))
                .await
                .unwrap();
            let first = read_frame(&mut stream).await.unwrap().unwrap();
            if threshold < 0 {
                assert_success(&first, &offline_profile("Steve"));
                configuration_end(&mut Transport::new(stream)).await;
            } else {
                let mut offer = vec![3];
                put_varint(threshold, &mut offer);
                assert_eq!(first, offer);
                // Inspect the frame before interpreting it in the new mode.
                let frame = read_frame(&mut stream).await.unwrap().unwrap();
                let mut reader = Reader::new(&frame);
                let declared = reader.varint().unwrap();
                let body = reader.remaining(1024).unwrap();
                if threshold == 0 {
                    use std::io::Read;
                    assert!(declared > 0);
                    let mut decoded = Vec::new();
                    flate2::read::ZlibDecoder::new(body.as_slice())
                        .take(1024)
                        .read_to_end(&mut decoded)
                        .unwrap();
                    assert_eq!(decoded.len(), declared as usize);
                    assert_success(&decoded, &offline_profile("Steve"));
                } else {
                    assert_eq!(declared, 0);
                    assert_success(&body, &offline_profile("Steve"));
                }
                let mut client = Transport::new(stream);
                client.enable_compression(threshold as usize).unwrap();
                configuration_end(&mut client).await;
            }
            stop_server(stop, task).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn login_codecs_reject_truncated_oversized_and_trailing_fields() {
    assert!(decode(State::Login, &login_start("a")[..3]).is_err());
    let mut body = login_start("Notch");
    body.push(0);
    assert!(decode(State::Login, &body).is_err());
    assert!(decode(State::Login, &[3, 0]).is_err());
    assert!(decode(State::Login, &[1, 0x81, 0x01]).is_err()); // >128 bytes
    assert!(decode(State::Login, &[1, 0xff, 0xff, 0xff, 0xff, 0x0f]).is_err());
    assert!(decode(State::Login, &[1, 128, 1, 0]).is_err()); // short ciphertext
    assert!(!Config::default().offline_mode);
    let parsed: Config = toml::from_str("offline_mode = true").unwrap();
    assert!(parsed.offline_mode);
}

#[tokio::test]
async fn encrypted_framing_preserves_cipher_state_across_fragmented_packets() {
    timeout(Duration::from_secs(5), async {
        let (left, right) = tokio::io::duplex(1);
        let secret = [17; 16];
        let mut writer = Transport::new(left);
        let mut reader = Transport::new(right);
        writer.enable_encryption(&secret).unwrap();
        reader.enable_encryption(&secret).unwrap();
        assert!(reader.enable_encryption(&secret).is_err());
        let task = tokio::spawn(async move {
            writer.write_frame(&[0; 200]).await.unwrap();
            writer.write_frame(&[1, 2, 3]).await.unwrap();
            assert_eq!(writer.read_frame().await.unwrap().unwrap(), [4, 5, 6]);
        });
        assert_eq!(reader.read_frame().await.unwrap().unwrap(), [0; 200]);
        assert_eq!(reader.read_frame().await.unwrap().unwrap(), [1, 2, 3]);
        reader.write_frame(&[4, 5, 6]).await.unwrap();
        task.await.unwrap();
        assert!(reader.read_frame().await.unwrap().is_none());
    })
    .await
    .unwrap();
}
