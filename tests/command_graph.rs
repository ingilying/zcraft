use std::{
    io,
    sync::{Arc, Mutex},
};
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

#[derive(Debug)]
struct Pair;
impl ArgumentParser for Pair {
    fn parse(&self, reader: &mut StringReader<'_>) -> Result<ArgumentValue, CommandError> {
        let left = reader.read_word();
        if reader.read() != Some(u16::from(b':')) {
            return Err(reader.error("Expected colon"));
        }
        let right = reader.read_word();
        Ok(ArgumentValue::Custom(Arc::new((left, right))))
    }
}

#[test]
fn custom_parser_arguments_graph_path_and_suggestion_ranges() {
    use zcraft::protocol::{Encode, packets::play_chat::DeclareCommands};
    let mut b = CommandRegistryBuilder::with_builtins();
    let root = b
        .add_node(NodeId::ROOT, CommandNode::literal("custom"))
        .unwrap();
    let pair = b
        .add_node(
            root,
            CommandNode::argument("pair", ArgumentKind::Custom(Arc::new(Pair)))
                .executes(|c, _| {
                    let (left, right) = c.arguments.custom::<(String, String)>("pair").unwrap();
                    Ok(CommandResult::message(format!("{left} and {right}")))
                })
                .suggests(|c| {
                    vec![
                        c.suggest("one:two"),
                        c.suggest("one:two"),
                        Suggestion {
                            span: c.start + 1..c.input.len(),
                            text: "XYZ".into(),
                            tooltip: Some("suffix".into()),
                        },
                    ]
                }),
        )
        .unwrap();
    let registry = b.build().unwrap();
    let source = source(0);
    let parsed = registry.dispatch_at("/custom one:two", 1, &source);
    assert_eq!(registry.path(pair).unwrap(), ["custom", "pair"]);
    assert_eq!(registry.find_node(&["custom", "pair"]), Some(pair));
    assert_eq!(parsed.contexts()[0].arguments.span("pair"), Some(8..15));
    assert_eq!(parsed.execute(&mut Host).unwrap().feedback, ["one and two"]);
    let partial = registry.dispatch("custom a", &source);
    assert_eq!(
        partial.suggestions(8),
        [
            Suggestion {
                span: 7..8,
                text: "aXYZ".into(),
                tooltip: Some("suffix".into())
            },
            Suggestion {
                span: 7..8,
                text: "one:two".into(),
                tooltip: None
            },
        ]
    );
    assert!(registry.help(&source).contains("/custom <pair>"));
    let mut wire = Vec::new();
    DeclareCommands::new(&registry, &source)
        .encode(&mut wire)
        .unwrap();
    assert!(wire.windows(6).any(|chunk| chunk == b"custom"));
    assert!(!wire.windows(4).any(|chunk| chunk == b"pair"));
}

static COMPLETIONS: Mutex<Vec<(u8, bool, i32)>> = Mutex::new(Vec::new());
#[test]
fn execution_fork_counts_success_and_reports_each_result() {
    let mut b = CommandRegistryBuilder::new();
    b.result_consumer(|call, success, value| {
        COMPLETIONS
            .lock()
            .unwrap()
            .push((call.source.permission, success, value))
    });
    b.add_node(
        NodeId::ROOT,
        CommandNode::literal("probe").executes(|c, _| {
            Ok(if c.source.permission == 2 {
                CommandResult::failure("skip")
            } else {
                CommandResult::success(10)
            })
        }),
    )
    .unwrap();
    b.add_node(
        NodeId::ROOT,
        CommandNode::literal("expand").forks(NodeId::ROOT, |_| Ok((1..=3).map(source).collect())),
    )
    .unwrap();
    b.add_node(
        NodeId::ROOT,
        CommandNode::literal("blowup").redirects(
            NodeId::ROOT,
            Some(|_| Err(CommandError::new("no source", 0..0))),
        ),
    )
    .unwrap();
    let reg = b.build().unwrap();
    let j = reg
        .dispatch("expand probe", &source(0))
        .execute(&mut Host)
        .unwrap();
    assert!(j.success);
    assert_eq!(j.value, 2);
    assert_eq!(j.feedback, ["skip"]);
    assert_eq!(
        reg.dispatch("blowup probe", &source(4))
            .execute(&mut Host)
            .unwrap()
            .feedback,
        ["no source"]
    );
    assert_eq!(
        *COMPLETIONS.lock().unwrap(),
        [(1, true, 10), (2, false, 0), (3, true, 10), (4, false, 0)]
    );
}

#[test]
fn ambiguity_and_explicit_limits() {
    let mut b = CommandRegistryBuilder::new();
    b.limits(DispatchLimits {
        depth: 2,
        visits: 16,
        sources: 2,
    });
    let root = b
        .add_node(NodeId::ROOT, CommandNode::literal("go"))
        .unwrap();
    let left = b
        .add_node(root, CommandNode::argument("x", ArgumentKind::Word))
        .unwrap();
    let right = b
        .add_node(root, CommandNode::argument("y", ArgumentKind::Bool))
        .unwrap();
    b.add_node(
        NodeId::ROOT,
        CommandNode::literal("repeat").redirects(NodeId::ROOT, None),
    )
    .unwrap();
    let reg = b.build().unwrap();
    assert!(
        reg.find_ambiguities()
            .iter()
            .any(|a| a.first == right && a.second == left && a.inputs == ["true", "false"])
    );
    let error = reg
        .dispatch("repeat repeat repeat go true", &source(0))
        .error()
        .unwrap();
    assert_eq!(error.message, "Command parsing budget exceeded");
}
