use std::io;
use uuid::Uuid;
use zcraft::command::*;

struct Host;
impl CommandHost for Host {
    fn list_players(&mut self) -> io::Result<CommandResult> {
        unreachable!()
    }
    fn action(&mut self, _: Uuid, _: &str) -> io::Result<CommandResult> {
        unreachable!()
    }
    fn whisper(&mut self, _: Uuid, _: &str, _: &str) -> io::Result<CommandResult> {
        unreachable!()
    }
}
fn source(permission: u8) -> CommandSource {
    CommandSource {
        kind: SourceKind::Console,
        permission,
        origin: None,
    }
}
fn tree() -> CommandRegistry {
    let mut b = CommandRegistryBuilder::new();
    let root = NodeId::ROOT;
    let render = b
        .add_node(
            root,
            CommandNode::literal("render").executes(|_, _| Ok(CommandResult::success(2))),
        )
        .unwrap();
    b.add_node(
        render,
        CommandNode::literal("on").executes(|_, _| Ok(CommandResult::success(3))),
    )
    .unwrap();
    b.add_node(
        render,
        CommandNode::argument("flag", ArgumentKind::Bool).executes(|c, _| {
            Ok(CommandResult::success(
                if c.arguments.boolean("flag").unwrap() {
                    11
                } else {
                    12
                },
            ))
        }),
    )
    .unwrap();
    assert_eq!(
        b.add_node(root, CommandNode::literal("render")).unwrap(),
        render
    );
    b.add_node(
        render,
        CommandNode::literal("off")
            .requires(|s| s.permission >= 2)
            .executes(|_, _| Ok(CommandResult::success(4))),
    )
    .unwrap();
    b.add_node(root, CommandNode::literal("alias").redirects(render, None))
        .unwrap();
    b.add_node(
        root,
        CommandNode::literal("as")
            .redirects(root, Some(|c| Ok(vec![source(c.source.permission + 1)]))),
    )
    .unwrap();
    b.add_node(
        root,
        CommandNode::literal("fork").forks(root, |_| Ok((1..=3).map(source).collect())),
    )
    .unwrap();
    b.add_node(
        root,
        CommandNode::literal("source")
            .executes(|c, _| Ok(CommandResult::success(c.source.permission.into()))),
    )
    .unwrap();
    let choose = b.add_node(root, CommandNode::literal("choose")).unwrap();
    let num = b
        .add_node(
            choose,
            CommandNode::argument("num", ArgumentKind::Integer { min: -4, max: 4 }),
        )
        .unwrap();
    b.add_node(
        num,
        CommandNode::literal("end")
            .executes(|c, _| Ok(CommandResult::success(c.arguments.integer("num").unwrap()))),
    )
    .unwrap();
    b.add_node(
        choose,
        CommandNode::argument("word", ArgumentKind::Word).executes(|c, _| {
            Ok(CommandResult::success(
                c.arguments.text("word").unwrap().encode_utf16().count() as i32,
            ))
        }),
    )
    .unwrap();
    for (literal, kind) in [
        ("quote", ArgumentKind::String),
        ("tail", ArgumentKind::Greedy),
    ] {
        let node = b.add_node(root, CommandNode::literal(literal)).unwrap();
        b.add_node(
            node,
            CommandNode::argument("text", kind).executes(|c, _| {
                Ok(CommandResult::success(
                    c.arguments.text("text").unwrap().encode_utf16().count() as i32,
                ))
            }),
        )
        .unwrap();
    }
    for (literal, kind) in [
        (
            "long",
            ArgumentKind::Long {
                min: -5_000_000_000,
                max: 5_000_000_000,
            },
        ),
        ("float", ArgumentKind::Float { min: -2., max: 2. }),
        ("double", ArgumentKind::Double { min: -2., max: 2. }),
    ] {
        let node = b.add_node(root, CommandNode::literal(literal)).unwrap();
        b.add_node(
            node,
            CommandNode::argument("n", kind).executes(|c, _| {
                Ok(CommandResult::success(
                    match c.arguments.get("n").unwrap() {
                        ArgumentValue::Long(v) => (v / 1_000_000_000) as i32,
                        ArgumentValue::Float(v) => (10. * v) as i32,
                        ArgumentValue::Double(v) => (10. * v) as i32,
                        _ => unreachable!(),
                    },
                ))
            }),
        )
        .unwrap();
    }
    b.build().unwrap()
}

fn unhex(s: &str) -> String {
    String::from_utf8(
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect(),
    )
    .unwrap()
}

#[test]
fn matches_brigiadier_1_3_10_fixture() {
    let registry = tree();
    let mut usages = [Vec::new(), Vec::new()];
    let mut smart = [Vec::new(), Vec::new()];
    let mut cases = 0;
    for line in include_str!("fixtures/brigadier-1.3.10.tsv").lines() {
        let fields: Vec<_> = line.split('\t').collect();
        let permission = fields[1].parse::<u8>().unwrap();
        let input = unhex(fields[2]);
        match fields[0] {
            "usage" => usages[usize::from(permission / 2)].push(input),
            "smart" => smart[usize::from(permission / 2)].push(input),
            "case" => {
                cases += 1;
                let parsed = registry.dispatch(&input, &source(permission));
                assert_eq!(
                    parsed.cursor(),
                    fields[3].parse::<usize>().unwrap(),
                    "{input:?} source {permission}"
                );
                let outcome: Vec<_> = fields[4].split(':').collect();
                if outcome[0] == "ok" {
                    assert!(parsed.error().is_none(), "{input:?} {:?}", parsed.error());
                    assert_eq!(
                        parsed.execute(&mut Host).unwrap().value,
                        outcome[1].parse::<i32>().unwrap(),
                        "{input:?}"
                    );
                } else {
                    let error = parsed
                        .error()
                        .unwrap_or_else(|| panic!("expected failure {input:?}"));
                    assert_eq!(
                        error.span.start,
                        outcome[1].parse::<usize>().unwrap(),
                        "{input:?}"
                    );
                    assert_eq!(error.message, unhex(outcome[2]), "{input:?}");
                }
                let expected: Vec<_> = fields[5]
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        let f: Vec<_> = s.split(':').collect();
                        Suggestion {
                            span: f[0].parse().unwrap()..f[1].parse().unwrap(),
                            text: unhex(f[2]),
                            tooltip: None,
                        }
                    })
                    .collect();
                assert_eq!(
                    parsed.suggestions(input.encode_utf16().count()),
                    expected,
                    "{input:?} source {permission}"
                );
            }
            _ => panic!("unknown fixture row"),
        }
    }
    assert_eq!(cases, 106);
    for (i, permission) in [0, 2].into_iter().enumerate() {
        assert_eq!(
            registry.all_usage(NodeId::ROOT, &source(permission), true),
            usages[i]
        );
        assert_eq!(
            registry
                .smart_usage(NodeId::ROOT, &source(permission))
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>(),
            smart[i]
        );
    }
}

#[test]
#[ignore = "requires Python, Java, and the verified 1.21.1 server JAR"]
fn regenerate_against_reference_jar() {
    let jar = std::env::var("ZCRAFT_BRIGADIER_SERVER_JAR")
        .unwrap_or_else(|_| "target/zcraft-data/1.21.1/server.jar".into());
    let output = std::process::Command::new("python3")
        .args(["tools/check-brigadier.py", &jar])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        include_bytes!("fixtures/brigadier-1.3.10.tsv")
    );
}
