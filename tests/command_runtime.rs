use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;
use zcraft::command::*;

fn console() -> SourceStack {
    SourceStack::new(CommandSource {
        kind: SourceKind::Console,
        permission: 2,
        origin: None,
    })
}
fn block() -> SourceStack {
    SourceStack::new(CommandSource {
        kind: SourceKind::CommandBlock,
        permission: 2,
        origin: None,
    })
}
fn dimension(id: &str, scale: f64) -> Dimension {
    Dimension {
        id: id.into(),
        coordinate_scale: scale,
    }
}
type ResultTrace = Arc<Mutex<Vec<(bool, i32)>>>;

fn trace() -> (ResultTrace, CommandCallback) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let target = events.clone();
    (
        events,
        Arc::new(move |success, value| target.lock().unwrap().push((success, value))),
    )
}

#[derive(Default)]
struct Host {
    stale: AtomicBool,
    actions: Vec<Uuid>,
    lists: usize,
}
impl CommandHost for Host {
    fn list_players(&mut self) -> io::Result<CommandResult> {
        self.lists += 1;
        Ok(CommandResult::message("list"))
    }
    fn action(&mut self, player: Uuid, _: &str) -> io::Result<CommandResult> {
        self.actions.push(player);
        Ok(CommandResult::message("action"))
    }
    fn whisper(&mut self, _: Uuid, _: &str, _: &str) -> io::Result<CommandResult> {
        unreachable!()
    }
    fn command_active(&self, _: &SourceStack) -> bool {
        !self.stale.load(Ordering::Relaxed)
    }
}

#[test]
fn source_transforms_are_checked_and_do_not_change_the_originator() {
    let player = Uuid::from_u128(1);
    let entity = Uuid::from_u128(2);
    let source = SourceStack::new(CommandSource::player(player))
        .positioned(dimension("minecraft:overworld", 1.0), [80.0, 64.0, -40.0])
        .unwrap();
    let derived = source
        .clone()
        .with_entity(Some(ExecutingEntity {
            uuid: entity,
            eye_height: 1.5,
        }))
        .unwrap()
        .with_rotation([90.0, -12.0])
        .unwrap()
        .with_anchor(Anchor::Eyes)
        .in_dimension(dimension("minecraft:the_nether", 8.0))
        .unwrap()
        .with_maximum_permission(4);
    assert_eq!(source.position(), Some([80.0, 64.0, -40.0]));
    assert_eq!(source.entity(), None);
    assert_eq!(source.rotation(), [0.0, 0.0]);
    assert_eq!(derived.authenticated_player(), Some(player));
    assert_eq!(derived.entity().unwrap().uuid, entity);
    assert_eq!(derived.rotation(), [90.0, -12.0]);
    assert_eq!(derived.anchored_position(), Some([10.0, 65.5, -5.0]));
    assert_eq!(
        derived.source().origin.as_ref().unwrap().world,
        "minecraft:the_nether"
    );
    assert!(!derived.has_permission(1));
    assert!(
        console()
            .with_permission(0)
            .with_maximum_permission(3)
            .has_permission(2)
    );
    assert!(
        !console()
            .with_permission(0)
            .with_maximum_permission(3)
            .has_permission(3)
    );
    assert!(source.clone().with_position([f64::NAN, 0.0, 0.0]).is_err());
    assert!(source.clone().with_rotation([f32::INFINITY, 0.0]).is_err());
    assert!(
        source
            .clone()
            .in_dimension(dimension("invalid", 0.0))
            .is_err()
    );
    assert!(
        source
            .with_entity(Some(ExecutingEntity {
                uuid: entity,
                eye_height: -1.0
            }))
            .is_err()
    );
}

fn add(builder: &mut CommandRegistryBuilder, name: &str, execute: Executor) {
    builder
        .add_node(NodeId::ROOT, CommandNode::literal(name).executes(execute))
        .unwrap();
}
fn enqueue(host: &mut dyn CommandHost, input: &str) {
    let _ = host.execution().unwrap().enqueue(input);
}
fn frame(
    host: &mut dyn CommandHost,
    source: &SourceStack,
    input: &str,
    callback: Option<CommandCallback>,
) {
    let _ = host
        .execution()
        .unwrap()
        .enqueue_frame(input, source.clone(), callback);
}

#[test]
fn new_actions_run_in_call_order_before_older_queued_work() {
    let mut builder = CommandRegistryBuilder::new();
    add(&mut builder, "start", |_, host| {
        enqueue(host, "branch");
        enqueue(host, "last");
        Ok(CommandResult::message("start"))
    });
    add(&mut builder, "branch", |_, host| {
        enqueue(host, "leaf");
        Ok(CommandResult::message("branch"))
    });
    add(&mut builder, "leaf", |_, _| {
        Ok(CommandResult::message("leaf"))
    });
    add(&mut builder, "last", |_, _| {
        Ok(CommandResult::message("last"))
    });
    let registry = builder.build().unwrap();
    let report = registry.run(
        "start",
        &console(),
        &mut Host::default(),
        ExecutionLimits::default(),
    );
    assert_eq!(report.abort, None);
    assert_eq!(report.result.feedback, ["start", "branch", "leaf", "last"]);
}

#[test]
fn frames_return_and_discard_pending_descendants() {
    let mut builder = CommandRegistryBuilder::new();
    add(&mut builder, "start", |call, host| {
        enqueue(host, "first");
        frame(host, call.stack.unwrap(), "child", None);
        enqueue(host, "last");
        Ok(CommandResult::message("start"))
    });
    add(&mut builder, "first", |_, _| {
        Ok(CommandResult::message("first"))
    });
    add(&mut builder, "last", |_, _| {
        Ok(CommandResult::message("last"))
    });
    add(&mut builder, "child", |call, host| {
        enqueue(host, "must_not_run");
        frame(host, call.stack.unwrap(), "must_not_run", None);
        host.execution().unwrap().return_from_frame(true, 27);
        Ok(CommandResult::message("child"))
    });
    add(&mut builder, "must_not_run", |_, _| {
        panic!("discarded work executed")
    });
    let (events, callback) = trace();
    let report = builder.build().unwrap().run(
        "start",
        &block().with_callback(callback),
        &mut Host::default(),
        ExecutionLimits::default(),
    );
    assert_eq!(report.abort, None);
    assert_eq!(report.result.feedback, ["start", "first", "child", "last"]);
    assert_eq!(report.result.value, 1);
    assert_eq!(
        *events.lock().unwrap(),
        [(true, 1), (true, 1), (true, 27), (true, 1)]
    );
}

#[test]
fn frame_results_and_source_callbacks_survive_silence_and_complete_once() {
    let mut builder = CommandRegistryBuilder::new();
    add(&mut builder, "start", |call, host| {
        frame(
            host,
            &call.stack.unwrap().clone().silent(),
            "leaf",
            Some(Arc::new(|success, value| {
                assert!(success);
                assert_eq!(value, 91);
            })),
        );
        Ok(CommandResult::message("visible"))
    });
    add(&mut builder, "leaf", |_, host| {
        host.execution().unwrap().return_from_frame(true, 91);
        Ok(CommandResult::message("hidden"))
    });
    let (a, ca) = trace();
    let (b, cb) = trace();
    let source = console().with_callback(ca).with_callback(cb);
    let registry = builder.build().unwrap();
    let report = registry.run(
        "start",
        &source,
        &mut Host::default(),
        ExecutionLimits::default(),
    );
    assert_eq!(report.result.feedback, ["visible"]);
    assert_eq!(*a.lock().unwrap(), [(true, 1), (true, 91)]);
    assert_eq!(*a.lock().unwrap(), *b.lock().unwrap());
    let result = registry.run(
        "missing",
        &source.silent(),
        &mut Host::default(),
        ExecutionLimits::default(),
    );
    assert!(!result.result.success && result.result.feedback.is_empty());
    assert_eq!(a.lock().unwrap().last(), Some(&(false, 0)));
    assert_eq!(a.lock().unwrap().len(), 3);
}

#[test]
fn fork_callbacks_report_each_execution_not_the_aggregate_count() {
    let mut builder = CommandRegistryBuilder::new();
    add(&mut builder, "leaf", |_, _| Ok(CommandResult::success(77)));
    builder
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("fan").redirects_stack(
                NodeId::ROOT,
                |_, source| Ok(vec![source.clone(), source.clone()]),
                true,
            ),
        )
        .unwrap();
    let (events, callback) = trace();
    let result = builder.build().unwrap().run(
        "fan leaf",
        &console().with_callback(callback),
        &mut Host::default(),
        ExecutionLimits::default(),
    );
    assert_eq!(result.abort, None);
    assert_eq!(result.result.value, 2);
    assert_eq!(*events.lock().unwrap(), [(true, 77), (true, 77)]);
}

#[test]
fn shared_budgets_limit_loops_queue_frames_and_redirect_expansion() {
    let mut builder = CommandRegistryBuilder::new();
    add(&mut builder, "cycle", |_, host| {
        enqueue(host, "cycle");
        Ok(CommandResult::success(1))
    });
    add(&mut builder, "queue", |_, host| {
        enqueue(host, "cycle");
        enqueue(host, "cycle");
        Ok(CommandResult::success(1))
    });
    add(&mut builder, "depth", |call, host| {
        frame(host, call.stack.unwrap(), "depth", None);
        Ok(CommandResult::success(1))
    });
    add(&mut builder, "leaf", |_, _| Ok(CommandResult::success(77)));
    builder
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("fan").redirects_stack(
                NodeId::ROOT,
                |_, source| Ok(vec![source.clone(), source.clone()]),
                true,
            ),
        )
        .unwrap();
    let registry = builder.build().unwrap();
    let mut host = Host::default();
    let defaults = ExecutionLimits::default();
    for (input, limits, failure) in [
        (
            "cycle",
            ExecutionLimits {
                cost: 7,
                ..defaults
            },
            ExecutionFailure::Cost,
        ),
        (
            "queue",
            ExecutionLimits {
                queued: 1,
                ..defaults
            },
            ExecutionFailure::Queue,
        ),
        (
            "depth",
            ExecutionLimits {
                frame_depth: 2,
                ..defaults
            },
            ExecutionFailure::Depth,
        ),
        (
            "fan fan leaf",
            ExecutionLimits {
                sources: 5,
                ..defaults
            },
            ExecutionFailure::Sources,
        ),
    ] {
        let report = registry.run(input, &block(), &mut host, limits);
        assert_eq!(report.abort, Some(failure), "{input}");
        assert!(!report.result.success);
        assert!(report.steps <= limits.cost);
    }
    let fork = registry.run("fan leaf", &console(), &mut host, defaults);
    assert_eq!(fork.result, CommandResult::success(2));
}

#[test]
fn execute_as_and_scheduling_cannot_replace_identity_or_elevate() {
    let mut builder = CommandRegistryBuilder::with_builtins();
    add(&mut builder, "probe", |call, host| {
        let stack = call.stack.unwrap();
        assert_eq!(stack.authenticated_player(), Some(Uuid::from_u128(1)));
        assert_eq!(stack.entity().unwrap().uuid, Uuid::from_u128(2));
        assert!(!stack.has_permission(1));
        assert!(!host.action(Uuid::from_u128(2), "impersonation")?.success);
        frame(host, &console(), "privileged", None);
        host.action(Uuid::from_u128(1), "real")
    });
    builder.register(
        Command::new("privileged", "", |_, _| panic!("escalated permission")).permission(1),
    );
    builder
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("as_other").redirects_stack(
                NodeId::ROOT,
                |_, _| {
                    let fake = SourceStack::new(CommandSource {
                        kind: SourceKind::Player(Uuid::from_u128(2)),
                        permission: 4,
                        origin: None,
                    })
                    .with_entity(Some(ExecutingEntity {
                        uuid: Uuid::from_u128(2),
                        eye_height: 1.5,
                    }))
                    .unwrap();
                    Ok(vec![fake])
                },
                false,
            ),
        )
        .unwrap();
    let registry = builder.build().unwrap();
    let mut host = Host::default();
    let report = registry.run(
        "as_other probe",
        &SourceStack::new(CommandSource::player(Uuid::from_u128(1))),
        &mut host,
        ExecutionLimits::default(),
    );
    assert_eq!(report.abort, None);
    assert_eq!(host.actions, [Uuid::from_u128(1)]);
    assert_eq!(report.result.feedback, ["action", "Permission denied."]);
}

#[test]
fn cancellation_discards_feedback_and_pending_work_without_rolling_back() {
    let mut builder = CommandRegistryBuilder::with_builtins();
    add(&mut builder, "start", |_, host| {
        enqueue(host, "list");
        Ok(CommandResult::message("stale"))
    });
    add(&mut builder, "cancel", |call, host| {
        frame(
            host,
            call.stack.unwrap(),
            "list",
            Some(Arc::new(|success, value| {
                assert!(!success);
                assert_eq!(value, 0);
            })),
        );
        host.execution().unwrap().cancel();
        Ok(CommandResult::message("must not appear"))
    });
    let registry = builder.build().unwrap();
    let mut host = Host::default();
    let failed = registry.run("cancel", &block(), &mut host, ExecutionLimits::default());
    assert_eq!(failed.abort, Some(ExecutionFailure::Cancelled));
    assert!(failed.result.feedback.is_empty());
    let stale = Arc::new(AtomicBool::new(false));
    struct CancelHost(Arc<AtomicBool>);
    impl CommandHost for CancelHost {
        fn list_players(&mut self) -> io::Result<CommandResult> {
            panic!("stale work executed")
        }
        fn action(&mut self, _: Uuid, _: &str) -> io::Result<CommandResult> {
            unreachable!()
        }
        fn whisper(&mut self, _: Uuid, _: &str, _: &str) -> io::Result<CommandResult> {
            unreachable!()
        }
        fn command_active(&self, _: &SourceStack) -> bool {
            !self.0.load(Ordering::Relaxed)
        }
    }
    let switch = stale.clone();
    let source = SourceStack::new(CommandSource::player(Uuid::nil()))
        .with_callback(Arc::new(move |_, _| switch.store(true, Ordering::Relaxed)));
    let report = registry.run(
        "start",
        &source,
        &mut CancelHost(stale),
        ExecutionLimits::default(),
    );
    assert_eq!(report.abort, Some(ExecutionFailure::Cancelled));
    assert!(report.result.feedback.is_empty());
    assert_eq!(host.lists, 0);
}

#[test]
fn rejects_invalid_origins_and_blocks_recursive_dispatch_on_the_same_host() {
    let mut builder = CommandRegistryBuilder::new();
    add(&mut builder, "nested", |call, host| {
        let report = call.registry.run(
            "nested",
            call.stack.unwrap(),
            host,
            ExecutionLimits::default(),
        );
        assert_eq!(report.abort, Some(ExecutionFailure::Host));
        assert_eq!(report.steps, 0);
        assert!(
            call.registry
                .dispatch("nested", call.source)
                .execute(host)
                .is_err()
        );
        Ok(CommandResult::success(1))
    });
    let (events, callback) = trace();
    let source = SourceStack::new(CommandSource {
        kind: SourceKind::Console,
        permission: 1,
        origin: Some(Origin {
            world: "minecraft:overworld".into(),
            position: [f64::NAN, 0.0, 0.0],
        }),
    })
    .with_callback(callback);
    let registry = builder.build().unwrap();
    let mut host = Host::default();
    let bad = registry.run("nested", &source, &mut host, ExecutionLimits::default());
    assert_eq!(bad.abort, Some(ExecutionFailure::InvalidSource));
    assert_eq!(bad.steps, 0);
    assert_eq!(*events.lock().unwrap(), [(false, 0)]);
    assert!(
        registry
            .run("nested", &console(), &mut host, ExecutionLimits::default())
            .result
            .success
    );
}
