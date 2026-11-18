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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Arc::new(ServerContext {
        status: StatusService::new("test", 20),
        login,
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(serve_with_context(
        listener,
        Config::default(),
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
    client.write_frame(&[3]).await.unwrap();
    let body = client.read_frame().await.unwrap().unwrap();
    let mut reader = Reader::new(&body);
    assert_eq!(reader.varint().unwrap(), 2); // Configuration disconnect ID
    assert_eq!(reader.take(1).unwrap(), &[8]); // Anonymous TAG_String
    let len = reader.u16().unwrap() as usize;
    assert!(
        std::str::from_utf8(reader.take(len).unwrap())
            .unwrap()
            .contains("world entry is not implemented")
    );
    reader.finish().unwrap();
    assert!(client.read_frame().await.unwrap().is_none());
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
