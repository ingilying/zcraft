use zcraft::{
    command::{
        ArgumentKind, Command, CommandRegistryBuilder, CommandResult, CommandSource, Origin,
        SourceKind,
    },
    context::ServerContext,
    handler::{
        configuration::ConfigurationService, login::LoginService, play::PlayService,
        status::StatusService,
    },
};

#[test]
fn custom_registration_is_executable_from_console_and_block_without_a_session() {
    let mut builder = CommandRegistryBuilder::with_builtins();
    builder.register(
        Command::new("count", "return a typed value", |call, _| {
            let mut result = CommandResult::success(call.arguments.integer("value").unwrap());
            if let Some(origin) = &call.source.origin {
                result
                    .feedback
                    .push(format!("{} at {:?}", origin.world, origin.position));
            }
            Ok(result)
        })
        .alias("result")
        .permission(2)
        .argument("value", ArgumentKind::Integer { min: 0, max: 100 }),
    );
    let server = ServerContext {
        status: StatusService::new("commands", 20),
        login: LoginService::offline(),
        configuration: ConfigurationService::new(),
        play: PlayService::new()
            .unwrap()
            .with_commands(builder.build().unwrap()),
    };
    let console = CommandSource {
        kind: SourceKind::Console,
        permission: 4,
        origin: None,
    };
    assert_eq!(
        server.execute_command(&console, "count 42").unwrap(),
        CommandResult::success(42)
    );
    let block = CommandSource {
        kind: SourceKind::CommandBlock,
        permission: 2,
        origin: Some(Origin {
            world: "minecraft:overworld".into(),
            position: [8.0, 64.0, 8.0],
        }),
    };
    let result = server.execute_command(&block, "result 7").unwrap();
    assert!(result.success);
    assert_eq!(result.value, 7);
    assert_eq!(result.feedback, ["minecraft:overworld at [8.0, 64.0, 8.0]"]);
    assert!(!server.execute_command(&block, "me hello").unwrap().success);
    let list = server.execute_command(&console, "list").unwrap();
    assert!(list.success);
    assert_eq!(list.value, 0);
    assert_eq!(list.feedback, ["Online (0): "]);
    assert!(
        server.execute_command(&console, "help").unwrap().feedback[0].contains("/count <value>")
    );
    let unprivileged = CommandSource::player(uuid::Uuid::nil());
    assert!(
        !server
            .execute_command(&unprivileged, "result 7")
            .unwrap()
            .success
    );
    // Public trusted execution still cannot impersonate a player absent from the registry.
    let unavailable = server
        .execute_command(&unprivileged, "msg Alice secret")
        .unwrap();
    assert!(!unavailable.success);
    assert!(unavailable.feedback.is_empty());
}

#[test]
fn builtin_op_commands_control_time_weather_blocks_ops_and_shutdown() {
    let server = ServerContext {
        status: StatusService::new("commands", 20),
        login: LoginService::offline(),
        configuration: ConfigurationService::new(),
        play: PlayService::new().unwrap(),
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    server.play.set_shutdown_trigger(stop_tx);

    let console = CommandSource {
        kind: SourceKind::Console,
        permission: 4,
        origin: None,
    };
    let player = CommandSource::player(uuid::Uuid::from_u128(1));

    // Unprivileged player cannot execute OP commands.
    for cmd in [
        "time set night",
        "weather rain",
        "setblock 0 64 0 stone",
        "op Alice",
        "stop",
    ] {
        assert!(
            !server.execute_command(&player, cmd).unwrap().success,
            "{cmd}"
        );
    }

    // /time set, add, query
    assert_eq!(
        server
            .execute_command(&console, "time set night")
            .unwrap()
            .value,
        13000
    );
    assert_eq!(
        server
            .execute_command(&console, "time add 500")
            .unwrap()
            .value,
        13500
    );
    assert_eq!(
        server
            .execute_command(&console, "time query daytime")
            .unwrap()
            .value,
        13500
    );

    // /weather thunder & clear
    assert!(
        server
            .execute_command(&console, "weather thunder 60")
            .unwrap()
            .success
    );
    let weather = server.play.weather();
    assert!(weather.raining && weather.thundering);
    assert!(
        server
            .execute_command(&console, "weather clear")
            .unwrap()
            .success
    );
    assert!(!server.play.weather().raining);

    // /setblock & /fill
    assert!(
        server
            .execute_command(&console, "setblock 8 64 8 glowstone")
            .unwrap()
            .success
    );
    let filled = server
        .execute_command(&console, "fill 0 64 0 1 65 1 stone")
        .unwrap();
    assert!(filled.success);
    assert_eq!(filled.value, 8);

    // /op & /deop
    let alice_id = uuid::Uuid::from_u128(99);
    assert_eq!(server.play.permission_for(alice_id, "Alice"), 0);
    assert!(
        server
            .execute_command(&console, "op Alice")
            .unwrap()
            .success
    );
    assert_eq!(server.play.permission_for(alice_id, "Alice"), 4);
    assert!(
        !server
            .execute_command(&console, "op Alice")
            .unwrap()
            .success
    );
    assert!(
        server
            .execute_command(&console, "deop Alice")
            .unwrap()
            .success
    );
    assert_eq!(server.play.permission_for(alice_id, "Alice"), 0);

    // /stop
    assert!(!*stop_rx.borrow());
    assert!(server.execute_command(&console, "stop").unwrap().success);
    assert!(*stop_rx.borrow());
}
