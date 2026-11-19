mod common;

use common::negotiate_compression;
use std::{future::pending, io, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;
use zcraft::{
    auth::offline_profile,
    config::Config,
    context::ServerContext,
    handler::{
        configuration::ConfigurationService, login::LoginService, play::PlayService,
        status::StatusService,
    },
    network::serve_with_context,
    protocol::{Clientbound, Reader, packets::play::WorldBorder, put_string, put_varint},
    transport::Transport,
};

fn services(max_players: u32) -> Arc<ServerContext> {
    Arc::new(ServerContext {
        status: StatusService::new("test", max_players),
        login: LoginService::offline(),
        play: PlayService::new().unwrap(),
        configuration: ConfigurationService::new().unwrap(),
    })
}

async fn start(
    config: Config,
) -> (
    Arc<ServerContext>,
    std::net::SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = services(config.max_players);
    let (stop, shutdown) = oneshot::channel();
    let task = tokio::spawn(serve_with_context(
        listener,
        config,
        server.clone(),
        async move {
            let _ = shutdown.await;
        },
    ));
    (server, addr, stop, task)
}

type Client = Transport<TcpStream>;

async fn connect(addr: std::net::SocketAddr, next_state: i32) -> Client {
    let mut client = Transport::new(TcpStream::connect(addr).await.unwrap());
    let mut handshake = vec![0];
    put_varint(767, &mut handshake);
    put_string("localhost", &mut handshake).unwrap();
    handshake.extend_from_slice(&addr.port().to_be_bytes());
    put_varint(next_state, &mut handshake);
    client.write_frame(&handshake).await.unwrap();
    client
}

async fn read(client: &mut Client) -> Option<Vec<u8>> {
    timeout(Duration::from_secs(3), client.read_frame())
        .await
        .unwrap()
        .unwrap()
}

async fn login(addr: std::net::SocketAddr, name: &str, claimed: Uuid) -> Client {
    let mut client = connect(addr, 2).await;
    let mut start = vec![0];
    put_string(name, &mut start).unwrap();
    start.extend_from_slice(claimed.as_bytes());
    client.write_frame(&start).await.unwrap();
    client
}

async fn assert_rejected(mut client: Client, reason: &str) {
    let reply = read(&mut client).await.unwrap();
    let mut reader = Reader::new(&reply);
    assert_eq!(reader.varint().unwrap(), 0);
    assert!(reader.string(32767).unwrap().contains(reason));
    reader.finish().unwrap();
    assert!(read(&mut client).await.is_none());
}

async fn assert_status(addr: std::net::SocketAddr, online: u32, max: u32) {
    let mut client = connect(addr, 1).await;
    client.write_frame(&[0]).await.unwrap();
    let body = read(&mut client).await.unwrap();
    let mut reader = Reader::new(&body);
    assert_eq!(reader.varint().unwrap(), 0);
    let json: serde_json::Value = serde_json::from_str(&reader.string(32767).unwrap()).unwrap();
    assert_eq!(json["players"]["online"], online);
    assert_eq!(json["players"]["max"], max);
}

async fn wait_online(server: &ServerContext, count: u32) {
    timeout(Duration::from_secs(3), async {
        while server.players().online_count() != count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

async fn enter_play_unconfirmed(client: &mut Client) -> i32 {
    client.write_frame(&[3]).await.unwrap();
    assert_eq!(read(client).await.unwrap()[0], 0x0c);
    assert_eq!(read(client).await.unwrap(), [0x0e, 0]);
    client.write_frame(&[7, 0]).await.unwrap();
    loop {
        let body = read(client).await.unwrap();
        match body[0] {
            3 => break,
            7 | 0x0d => {}
            other => panic!("unexpected Configuration packet {other:#x}"),
        }
    }
    client.write_frame(&[3]).await.unwrap();
    let join = read(client).await.unwrap();
    assert_eq!(join[0], 0x2b);
    let id = i32::from_be_bytes(join[1..5].try_into().unwrap());
    loop {
        if read(client).await.unwrap()[0] == 0x40 {
            break;
        }
    }
    id
}

async fn enter_play(client: &mut Client) {
    enter_play_unconfirmed(client).await;
    client.write_frame(&[0, 0]).await.unwrap();
}

async fn expect_peer(client: &mut Client, name: &str, id: i32) {
    let uuid = offline_profile(name).uuid;
    let profile = read(client).await.unwrap();
    assert_eq!(&profile[..3], &[0x3e, 0x0d, 1]);
    assert_eq!(&profile[3..19], uuid.as_bytes());
    assert_eq!(Reader::new(&profile[19..]).string(16).unwrap(), name);
    let spawn = read(client).await.unwrap();
    let mut spawn = Reader::new(&spawn);
    assert_eq!(spawn.varint().unwrap(), 1);
    assert_eq!(spawn.varint().unwrap(), id);
    assert_eq!(spawn.take(16).unwrap(), uuid.as_bytes());
    assert_eq!(spawn.varint().unwrap(), 128);
}

fn position_look(x: f64, yaw: f32, pitch: f32) -> Vec<u8> {
    let mut body = vec![0x1b];
    for value in [x, 65.0, 8.5] {
        body.extend(value.to_be_bytes());
    }
    body.extend(yaw.to_be_bytes());
    body.extend(pitch.to_be_bytes());
    body.push(0);
    body
}

fn entity_update(id: i32, packet_id: u8, rest: &[u8]) -> Vec<u8> {
    let mut body = vec![packet_id];
    put_varint(id, &mut body);
    body.extend(rest);
    body
}

#[tokio::test]
async fn play_peers_sync_spawn_pose_leave_and_reconnect_over_tcp() {
    let (server, addr, stop, task) = start(Config::default()).await;
    let mut alice = login(addr, "Alice", Uuid::nil()).await;
    negotiate_compression(&mut alice).await;
    assert_eq!(read(&mut alice).await.unwrap()[0], 2);
    let alice_id = enter_play_unconfirmed(&mut alice).await;
    alice.write_frame(&[0, 0]).await.unwrap();

    let mut bob = login(addr, "Bob", Uuid::nil()).await;
    negotiate_compression(&mut bob).await;
    assert_eq!(read(&mut bob).await.unwrap()[0], 2);
    let bob_id = enter_play_unconfirmed(&mut bob).await;
    assert_ne!(alice_id, bob_id);
    bob.write_frame(&position_look(20.0, 45.0, 45.0))
        .await
        .unwrap();
    wait_online(&server, 2).await;
    // Ping the generic broadcast queue as a barrier: no peer events precede it.
    let barrier = Clientbound::from(WorldBorder);
    let body = barrier.encode().unwrap();
    assert_eq!(server.players().broadcast(&barrier).unwrap().queued, 2);
    assert_eq!(read(&mut alice).await.unwrap(), body);
    assert_eq!(read(&mut bob).await.unwrap(), body);
    bob.write_frame(&[0, 0]).await.unwrap();
    expect_peer(&mut alice, "Bob", bob_id).await;
    expect_peer(&mut bob, "Alice", alice_id).await;

    alice
        .write_frame(&position_look(12.0, 90.0, -45.0))
        .await
        .unwrap();
    let to = [12f64, 65.0, 8.5].map(f64::to_be_bytes).concat();
    let expected = entity_update(alice_id, 0x70, &[to, vec![64, 224, 0]].concat());
    assert_eq!(read(&mut bob).await.unwrap(), expected);
    assert_eq!(
        read(&mut bob).await.unwrap(),
        entity_update(alice_id, 0x48, &[64])
    );

    let mut look = vec![0x1c];
    look.extend(180f32.to_be_bytes());
    look.extend(0f32.to_be_bytes());
    look.push(0);
    alice.write_frame(&look).await.unwrap();
    assert_eq!(
        read(&mut bob).await.unwrap(),
        entity_update(alice_id, 0x30, &[128, 0, 0])
    );
    assert_eq!(
        read(&mut bob).await.unwrap(),
        entity_update(alice_id, 0x48, &[128])
    );

    alice
        .write_frame(&position_look(200.0, 90.0, 45.0))
        .await
        .unwrap();
    let correction = read(&mut alice).await.unwrap();
    assert_eq!(correction[0], 0x40);
    assert_eq!(correction.last(), Some(&1));
    alice
        .write_frame(&position_look(22.0, 90.0, 45.0))
        .await
        .unwrap();
    alice.write_frame(&[0, 1]).await.unwrap();
    let spawn = [8.5f64, 65.0, 8.5].map(f64::to_be_bytes).concat();
    assert_eq!(
        read(&mut bob).await.unwrap(),
        entity_update(alice_id, 0x70, &[spawn, vec![0, 0, 0]].concat())
    );
    assert_eq!(
        read(&mut bob).await.unwrap(),
        entity_update(alice_id, 0x48, &[0])
    );

    drop(alice);
    let mut removed = vec![0x42, 1];
    put_varint(alice_id, &mut removed);
    assert_eq!(read(&mut bob).await.unwrap(), removed);
    let mut removed_profile = vec![0x3d, 1];
    removed_profile.extend(offline_profile("Alice").uuid.as_bytes());
    assert_eq!(read(&mut bob).await.unwrap(), removed_profile);

    let mut alice = login(addr, "Alice", Uuid::nil()).await;
    negotiate_compression(&mut alice).await;
    assert_eq!(read(&mut alice).await.unwrap()[0], 2);
    let reconnected_id = enter_play_unconfirmed(&mut alice).await;
    assert_ne!(alice_id, reconnected_id);
    alice.write_frame(&[0, 0]).await.unwrap();
    expect_peer(&mut bob, "Alice", reconnected_id).await;
    expect_peer(&mut alice, "Bob", bob_id).await;
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn admission_status_broadcast_and_cleanup_follow_the_tcp_connection() {
    let (server, addr, stop, task) = start(Config {
        max_players: 1,
        ..Config::default()
    })
    .await;
    let uuid = offline_profile("Alice").uuid;
    let packet = Clientbound::from(WorldBorder);
    let mut first = login(addr, "Alice", Uuid::nil()).await;
    negotiate_compression(&mut first).await;
    let success = read(&mut first).await.unwrap();
    assert_eq!(success[0], 2);
    assert_eq!(&success[1..17], uuid.as_bytes());
    assert_status(addr, 0, 1).await;
    assert_eq!(server.players().broadcast(&packet).unwrap().queued, 0);
    // A different claimed UUID cannot evade the duplicate check; the same
    // claimed UUID on another username is classified as capacity, not identity.
    assert_rejected(
        login(addr, "Alice", Uuid::from_u128(1)).await,
        "already connected",
    )
    .await;
    assert_rejected(login(addr, "Bob", Uuid::nil()).await, "server is full").await;
    enter_play(&mut first).await;
    wait_online(&server, 1).await;
    assert_status(addr, 1, 1).await;
    assert_eq!(server.players().online_ids(), vec![uuid]);
    assert_eq!(server.players().broadcast(&packet).unwrap().queued, 1);
    assert_eq!(read(&mut first).await.unwrap(), packet.encode().unwrap());
    drop(first);
    wait_online(&server, 0).await;
    assert_status(addr, 0, 1).await;

    let mut reconnect = login(addr, "Alice", Uuid::nil()).await;
    negotiate_compression(&mut reconnect).await;
    assert_eq!(read(&mut reconnect).await.unwrap()[0], 2);
    enter_play(&mut reconnect).await;
    wait_online(&server, 1).await;
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(server.players().online_count(), 0);
    let closed = timeout(Duration::from_secs(3), reconnect.read_frame())
        .await
        .unwrap();
    assert!(
        matches!(closed, Ok(None))
            || closed.is_err_and(|error| error.kind() == io::ErrorKind::ConnectionReset)
    );
}

#[tokio::test]
async fn timed_out_login_releases_the_reserved_slot() {
    let (_server, addr, stop, task) = start(Config {
        max_players: 1,
        io_timeout: Duration::from_secs(1),
        ..Config::default()
    })
    .await;
    let mut first = login(addr, "Alice", Uuid::nil()).await;
    negotiate_compression(&mut first).await;
    assert_eq!(read(&mut first).await.unwrap()[0], 2);
    // Leave Login Acknowledged unsent until the read deadline expires.
    assert!(read(&mut first).await.is_none());
    let mut second = login(addr, "Alice", Uuid::nil()).await;
    negotiate_compression(&mut second).await;
    assert_eq!(read(&mut second).await.unwrap()[0], 2);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn context_cannot_silently_override_configured_capacity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let error = serve_with_context(listener, Config::default(), services(1), pending())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
