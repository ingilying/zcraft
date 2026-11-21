use super::*;
use crate::protocol::{Encode, Reader, packets::play_chat::DeclareCommands};

#[derive(Default)]
struct Host {
    actions: usize,
    whispers: usize,
}
impl CommandHost for Host {
    fn list_players(&mut self) -> io::Result<CommandResult> {
        Ok(CommandResult::message("Online (0): "))
    }
    fn action(&mut self, _: Uuid, _: &str) -> io::Result<CommandResult> {
        self.actions += 1;
        Ok(CommandResult::success(1))
    }
    fn whisper(&mut self, _: Uuid, _: &str, _: &str) -> io::Result<CommandResult> {
        self.whispers += 1;
        Ok(CommandResult::success(1))
    }
}
fn echo(call: &Invocation<'_>, _: &mut dyn CommandHost) -> io::Result<CommandResult> {
    Ok(CommandResult {
        success: call.arguments.boolean("enabled").unwrap(),
        value: call.arguments.integer("amount").unwrap(),
        feedback: vec![call.arguments.text("name").unwrap().into()],
    })
}
fn source(kind: SourceKind, permission: u8) -> CommandSource {
    CommandSource {
        kind,
        permission,
        origin: None,
    }
}
fn registry() -> CommandRegistry {
    let mut builder = CommandRegistryBuilder::with_builtins();
    builder.register(
        Command::new("echo", "test typed arguments", echo)
            .alias("say_number")
            .permission(2)
            .argument("name", ArgumentKind::String)
            .argument("amount", ArgumentKind::Integer { min: -5, max: 5 })
            .argument("enabled", ArgumentKind::Bool),
    );
    builder.build().unwrap()
}

#[test]
fn parser_has_types_quotes_utf8_spans_and_bounded_input() {
    let registry = registry();
    let source = source(SourceKind::Console, 4);
    let result = registry
        .execute(
            " /say_number \"名字 \\\"hi\\\"\\\\\" -5 true ",
            &source,
            &mut Host::default(),
        )
        .unwrap();
    assert_eq!(
        result,
        CommandResult {
            success: true,
            value: -5,
            feedback: vec!["名字 \"hi\"\\".into()]
        }
    );
    for (input, marker) in [
        ("echo x 6 true", "6"),
        ("echo x 0 yes", "yes"),
        ("echo x 0 true extra", "extra"),
    ] {
        let error = registry.parse(input, &source).err().unwrap();
        assert_eq!(&input[error.span], marker);
        assert!(error.usage.unwrap().contains("<amount>"));
    }
    for invalid in [
        "echo \"unfinished",
        "echo \"bad\\x\" 0 true",
        "echo \"x\"tail 0 true",
        "echo x 0",
        "echo x 2147483648 true",
        "echo unquoted名字 0 true",
    ] {
        assert!(registry.parse(invalid, &source).is_err(), "{invalid}");
    }
    let long = format!("me {}", "x".repeat(MAX_COMMAND_BYTES));
    assert!(
        registry
            .parse(&long, &CommandSource::player(Uuid::nil()))
            .is_err()
    );
    assert!(registry.parse("help\nlist", &source).is_err());
}

#[test]
fn permissions_aliases_help_and_sources_share_one_definition() {
    let registry = registry();
    for kind in [
        SourceKind::Console,
        SourceKind::CommandBlock,
        SourceKind::Player(Uuid::nil()),
    ] {
        let low = source(kind.clone(), 0);
        for name in ["echo", "say_number"] {
            let result = registry
                .execute(&format!("{name} x 1 true"), &low, &mut Host::default())
                .unwrap();
            assert!(!result.success);
            assert_eq!(result.value, 0);
            assert_eq!(result.feedback, ["Permission denied."]);
        }
        assert!(!registry.help(&low).contains("/echo"));
        let high = source(kind, 2);
        assert!(
            registry
                .help(&high)
                .contains("/echo <name> <amount> <enabled>")
        );
        assert!(
            !registry
                .execute("echo x 1 false", &high, &mut Host::default())
                .unwrap()
                .success
        );
    }
    for kind in [SourceKind::Console, SourceKind::CommandBlock] {
        let source = source(kind, 4);
        let mut host = Host::default();
        for input in ["me hello", "msg Alice secret", "tell Alice secret"] {
            let result = registry.execute(input, &source, &mut host).unwrap();
            assert!(!result.success);
            assert_eq!(result.feedback, ["This command requires a player."]);
        }
        assert_eq!((host.actions, host.whispers), (0, 0));
        assert!(
            registry
                .execute("help", &source, &mut host)
                .unwrap()
                .success
        );
        assert!(
            registry
                .execute("list", &source, &mut host)
                .unwrap()
                .success
        );
    }
}

#[test]
fn invalid_definitions_fail_before_runtime() {
    let command = || Command::new("test", "", |_, _| Ok(CommandResult::success(1)));
    for bad in [
        command().alias("test"),
        command().alias("Bad"),
        command()
            .argument("a", ArgumentKind::Greedy)
            .argument("b", ArgumentKind::Word),
        command()
            .argument("a", ArgumentKind::Bool)
            .argument("a", ArgumentKind::Word),
        command().argument("a", ArgumentKind::Integer { min: 5, max: 1 }),
    ] {
        let mut builder = CommandRegistryBuilder::new();
        builder.register(bad);
        assert!(builder.build().is_err());
    }
    let mut builder = CommandRegistryBuilder::new();
    builder.register(command()).register(command());
    assert!(builder.build().is_err());
    let mut builder = CommandRegistryBuilder::new();
    for i in 0..1024 {
        builder.register(Command::new(format!("c{i}"), "", |_, _| {
            Ok(CommandResult::success(1))
        }));
    }
    assert!(builder.build().is_err());
}

#[test]
fn wire_tree_uses_validated_arguments_and_permission_filter() {
    let registry = registry();
    for permission in [0, 2] {
        let source = source(SourceKind::Player(Uuid::nil()), permission);
        let mut bytes = Vec::new();
        DeclareCommands::new(&registry, &source)
            .encode(&mut bytes)
            .unwrap();
        let mut r = Reader::new(&bytes);
        let count = r.varint().unwrap();
        let mut names = Vec::new();
        let mut types = Vec::new();
        for index in 0..count {
            let flags = r.take(1).unwrap()[0];
            let children = r.varint().unwrap();
            for _ in 0..children {
                let child = r.varint().unwrap();
                assert!(child > 0 && child < count);
            }
            if index == 0 {
                assert_eq!(flags, 0);
                continue;
            }
            names.push(r.string(32).unwrap());
            if flags & 3 == 2 {
                let parser = r.varint().unwrap();
                types.push(parser);
                match parser {
                    0 => {}
                    3 => {
                        assert_eq!(r.take(1).unwrap(), [3]);
                        assert_eq!(
                            r.take(8).unwrap(),
                            [&(-5i32).to_be_bytes()[..], &5i32.to_be_bytes()[..]].concat()
                        );
                    }
                    5 => {
                        assert!((0..=2).contains(&r.varint().unwrap()));
                    }
                    _ => panic!("unexpected parser"),
                }
            }
        }
        assert_eq!(r.varint().unwrap(), 0);
        r.finish().unwrap();
        assert_eq!(names.iter().any(|name| name == "echo"), permission == 2);
        assert_eq!(
            names.iter().any(|name| name == "say_number"),
            permission == 2
        );
        assert_eq!(types.contains(&3), permission == 2);
    }
}
