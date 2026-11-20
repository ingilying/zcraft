//! Opt-in cross-implementation wire check; no Minecraft account is used.
//! NODE_PATH=/path/to/node_modules cargo test --test client_compat -- --ignored --nocapture
use std::{process::Command, sync::Arc};
use tokio::{net::TcpListener, sync::oneshot};
use zcraft::{
    config::{Config, GameMode},
    context::ServerContext,
    handler::{
        configuration::ConfigurationService, login::LoginService, play::PlayService,
        status::StatusService,
    },
    network::serve_with_context,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Node.js, minecraft-protocol and prismarine-chunk via NODE_PATH; takes 34 seconds"]
async fn independent_protocol_client_decodes_world_and_survives_login_deadline() {
    check_client("check-world.cjs", GameMode::Spectator).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Node.js and minecraft-protocol via NODE_PATH"]
async fn independent_protocol_clients_decode_multiplayer() {
    check_client("check-multiplayer.cjs", GameMode::Spectator).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Node.js, minecraft-protocol and prismarine-chunk via NODE_PATH"]
async fn independent_protocol_clients_edit_and_reload_blocks() {
    check_client("check-blocks.cjs", GameMode::Creative).await;
}

async fn check_client(script: &'static str, mode: GameMode) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = Arc::new(ServerContext {
        status: StatusService::new("test", 20),
        login: LoginService::offline(),
        configuration: ConfigurationService::new().unwrap(),
        play: PlayService::with_options(Config::default().max_view_distance, mode).unwrap(),
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(serve_with_context(
        listener,
        Config {
            game_mode: mode,
            ..Config::default()
        },
        server,
        async {
            let _ = stopped.await;
        },
    ));
    let output = tokio::task::spawn_blocking(move || {
        Command::new("node")
            .arg(format!("{}/tools/{script}", env!("CARGO_MANIFEST_DIR")))
            .env("ZCRAFT_TEST_PORT", port.to_string())
            .output()
    })
    .await
    .unwrap()
    .expect("Node.js must be installed");
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    println!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
