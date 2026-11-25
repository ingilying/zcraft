use super::{arguments::*, *};
use crate::config::GameMode;

pub(super) fn builder() -> CommandRegistryBuilder {
    let mut builder = CommandRegistryBuilder::new();
    builder.register(Command::new("help", "show commands", |call, _| {
        Ok(CommandResult::message(call.registry.help(call.source)))
    }));
    builder.register(Command::new("list", "list players", |_, host| {
        host.list_players()
    }));
    builder.register(
        Command::new("me", "broadcast an action", |call, host| {
            let SourceKind::Player(uuid) = call.source.kind else {
                unreachable!("player-only command")
            };
            host.action(
                uuid,
                call.arguments.text("action").expect("declared argument"),
            )
        })
        .player_only()
        .argument("action", ArgumentKind::Greedy),
    );
    builder.register(
        Command::new("msg", "send a private message", |call, host| {
            let SourceKind::Player(uuid) = call.source.kind else {
                unreachable!("player-only command")
            };
            let target = call.arguments.text("target").expect("declared argument");
            if !crate::auth::valid_username(target) {
                return Ok(CommandResult::failure("Invalid player name."));
            }
            host.whisper(
                uuid,
                target,
                call.arguments.text("message").expect("declared argument"),
            )
        })
        .alias("tell")
        .player_only()
        .argument("target", ArgumentKind::Word)
        .argument("message", ArgumentKind::Greedy),
    );

    register_time(&mut builder);
    register_weather(&mut builder);
    register_setblock(&mut builder);
    register_fill(&mut builder);
    register_tp(&mut builder);
    register_give(&mut builder);
    register_gamemode(&mut builder);
    register_op(&mut builder);
    register_kick(&mut builder);
    register_stop(&mut builder);
    builder
}

fn require_op2(source: &CommandSource) -> bool {
    source.permission >= 2
}
fn require_op3(source: &CommandSource) -> bool {
    source.permission >= 3
}
fn require_op4(source: &CommandSource) -> bool {
    source.permission >= 4
}

fn resolve_stack<'a>(call: &'a Invocation<'_>) -> Result<&'a SourceStack, CommandResult> {
    call.stack
        .ok_or_else(|| CommandResult::failure("Command runtime context is required."))
}

fn resolve_entities(
    call: &Invocation<'_>,
    host: &mut dyn CommandHost,
    arg: &str,
) -> Result<Vec<EntitySnapshot>, CommandResult> {
    let stack = resolve_stack(call)?;
    let query = call
        .arguments
        .custom::<EntityQuery>(arg)
        .expect("declared entity argument");
    let entities = host
        .command_entities()
        .map_err(|e| CommandResult::failure(e.to_string()))?;
    let mut rng = rand::thread_rng();
    query
        .resolve(stack, &entities, &mut rng)
        .map(|matched| matched.into_iter().cloned().collect())
        .map_err(|e| CommandResult::failure(e.to_string()))
}

fn register_time(b: &mut CommandRegistryBuilder) {
    let root = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("time").requires(require_op2),
        )
        .unwrap();
    let query = b.add_node(root, CommandNode::literal("query")).unwrap();
    b.add_node(
        query,
        CommandNode::literal("daytime").executes(|_, host| {
            let (_, day_time) = host.world_time()?;
            let value = (day_time.rem_euclid(crate::world::DAY_LENGTH)) as i32;
            Ok(CommandResult {
                success: true,
                value,
                feedback: vec![format!("The time is {value}.")],
            })
        }),
    )
    .unwrap();
    b.add_node(
        query,
        CommandNode::literal("gametime").executes(|_, host| {
            let (world_age, _) = host.world_time()?;
            let value = world_age as i32;
            Ok(CommandResult {
                success: true,
                value,
                feedback: vec![format!("The time is {world_age}.")],
            })
        }),
    )
    .unwrap();
    b.add_node(
        query,
        CommandNode::literal("day").executes(|_, host| {
            let (_, day_time) = host.world_time()?;
            let day = (day_time / crate::world::DAY_LENGTH) as i32;
            Ok(CommandResult {
                success: true,
                value: day,
                feedback: vec![format!("The time is day {day}.")],
            })
        }),
    )
    .unwrap();

    let set = b.add_node(root, CommandNode::literal("set")).unwrap();
    for (name, preset) in [
        ("day", 1000i64),
        ("noon", 6000),
        ("night", 13000),
        ("midnight", 18000),
    ] {
        let exec: Executor = match preset {
            1000 => |_, host| set_time_result(host, 1000),
            6000 => |_, host| set_time_result(host, 6000),
            13000 => |_, host| set_time_result(host, 13000),
            _ => |_, host| set_time_result(host, 18000),
        };
        b.add_node(set, CommandNode::literal(name).executes(exec))
            .unwrap();
    }
    b.add_node(
        set,
        CommandNode::argument(
            "time",
            ArgumentKind::Integer {
                min: 0,
                max: i32::MAX,
            },
        )
        .executes(|call, host| {
            let value = i64::from(call.arguments.integer("time").expect("declared argument"));
            set_time_result(host, value)
        }),
    )
    .unwrap();

    let add = b.add_node(root, CommandNode::literal("add")).unwrap();
    b.add_node(
        add,
        CommandNode::argument(
            "amount",
            ArgumentKind::Integer {
                min: 0,
                max: i32::MAX,
            },
        )
        .executes(|call, host| {
            let delta = i64::from(call.arguments.integer("amount").expect("declared argument"));
            let (_, current) = host.world_time()?;
            set_time_result(host, current.saturating_add(delta))
        }),
    )
    .unwrap();
}

fn set_time_result(host: &mut dyn CommandHost, target: i64) -> io::Result<CommandResult> {
    let time = host.set_time(target)?;
    Ok(CommandResult {
        success: true,
        value: time as i32,
        feedback: vec![format!("Set the time to {time}.")],
    })
}

fn register_weather(b: &mut CommandRegistryBuilder) {
    let root = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("weather").requires(require_op2),
        )
        .unwrap();
    let duration_arg = ArgumentKind::Integer {
        min: 1,
        max: 1_000_000,
    };

    let clear = b
        .add_node(
            root,
            CommandNode::literal("clear")
                .executes(|_, host| set_weather_result(host, false, false, 6000)),
        )
        .unwrap();
    b.add_node(
        clear,
        CommandNode::argument("duration", duration_arg.clone()).executes(|call, host| {
            let secs = call
                .arguments
                .integer("duration")
                .expect("declared argument") as u32;
            set_weather_result(host, false, false, secs.saturating_mul(20))
        }),
    )
    .unwrap();

    let rain = b
        .add_node(
            root,
            CommandNode::literal("rain")
                .executes(|_, host| set_weather_result(host, true, false, 6000)),
        )
        .unwrap();
    b.add_node(
        rain,
        CommandNode::argument("duration", duration_arg.clone()).executes(|call, host| {
            let secs = call
                .arguments
                .integer("duration")
                .expect("declared argument") as u32;
            set_weather_result(host, true, false, secs.saturating_mul(20))
        }),
    )
    .unwrap();

    let thunder = b
        .add_node(
            root,
            CommandNode::literal("thunder")
                .executes(|_, host| set_weather_result(host, true, true, 6000)),
        )
        .unwrap();
    b.add_node(
        thunder,
        CommandNode::argument("duration", duration_arg).executes(|call, host| {
            let secs = call
                .arguments
                .integer("duration")
                .expect("declared argument") as u32;
            set_weather_result(host, true, true, secs.saturating_mul(20))
        }),
    )
    .unwrap();
}

fn set_weather_result(
    host: &mut dyn CommandHost,
    raining: bool,
    thundering: bool,
    ticks: u32,
) -> io::Result<CommandResult> {
    host.set_weather(raining, thundering, ticks)?;
    let label = match (raining, thundering) {
        (false, _) => "clear",
        (true, false) => "rain",
        (true, true) => "rain & thunder",
    };
    Ok(CommandResult::message(format!(
        "Set the weather to {label}."
    )))
}

fn register_setblock(b: &mut CommandRegistryBuilder) {
    let root = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("setblock").requires(require_op2),
        )
        .unwrap();
    let pos = b
        .add_node(root, MinecraftArgument::BlockPos.node("pos"))
        .unwrap();
    b.add_node(
        pos,
        BlockArgument.node("block").executes(|call, host| {
            let stack = match resolve_stack(call) {
                Ok(s) => s,
                Err(r) => return Ok(r),
            };
            let pos = match call
                .arguments
                .custom::<BlockPos>("pos")
                .expect("declared argument")
                .resolve(stack)
            {
                Ok(p) => p,
                Err(e) => return Ok(CommandResult::failure(e.to_string())),
            };
            let spec = call
                .arguments
                .custom::<BlockStateSpec>("block")
                .expect("declared argument");
            let state = match spec.resolve_existing() {
                Ok(s) => s,
                Err(e) => return Ok(CommandResult::failure(e.to_string())),
            };
            if host.set_block(pos, state)? {
                Ok(CommandResult::message(format!(
                    "Changed the block at {}, {}, {}.",
                    pos[0], pos[1], pos[2]
                )))
            } else {
                Ok(CommandResult::failure(
                    "Could not set block (out of bounds or chunk edit cap reached).",
                ))
            }
        }),
    )
    .unwrap();
}

fn register_fill(b: &mut CommandRegistryBuilder) {
    let root = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("fill").requires(require_op2),
        )
        .unwrap();
    let from = b
        .add_node(root, MinecraftArgument::BlockPos.node("from"))
        .unwrap();
    let to = b
        .add_node(from, MinecraftArgument::BlockPos.node("to"))
        .unwrap();
    b.add_node(
        to,
        BlockArgument.node("block").executes(|call, host| {
            let stack = match resolve_stack(call) {
                Ok(s) => s,
                Err(r) => return Ok(r),
            };
            let from = match call
                .arguments
                .custom::<BlockPos>("from")
                .expect("declared argument")
                .resolve(stack)
            {
                Ok(p) => p,
                Err(e) => return Ok(CommandResult::failure(e.to_string())),
            };
            let to = match call
                .arguments
                .custom::<BlockPos>("to")
                .expect("declared argument")
                .resolve(stack)
            {
                Ok(p) => p,
                Err(e) => return Ok(CommandResult::failure(e.to_string())),
            };
            let state = match call
                .arguments
                .custom::<BlockStateSpec>("block")
                .expect("declared argument")
                .resolve_existing()
            {
                Ok(s) => s,
                Err(e) => return Ok(CommandResult::failure(e.to_string())),
            };
            match host.fill_blocks(from, to, state) {
                Ok(0) => Ok(CommandResult::failure("No blocks were filled.")),
                Ok(count) => Ok(CommandResult {
                    success: true,
                    value: count as i32,
                    feedback: vec![format!("Successfully filled {count} block(s).")],
                }),
                Err(e) => Ok(CommandResult::failure(e.to_string())),
            }
        }),
    )
    .unwrap();
}

fn register_tp(b: &mut CommandRegistryBuilder) {
    let root = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("tp").requires(require_op2),
        )
        .unwrap();
    // /tp <x> <y> <z> (self)
    b.add_node(
        root,
        MinecraftArgument::Vec3 {
            center_correct: true,
        }
        .node("location")
        .executes(|call, host| {
            let SourceKind::Player(uuid) = call.source.kind else {
                return Ok(CommandResult::failure(
                    "Specify a target player when teleporting from console.",
                ));
            };
            let stack = match resolve_stack(call) {
                Ok(s) => s,
                Err(r) => return Ok(r),
            };
            let pos = match call
                .arguments
                .custom::<Vec3>("location")
                .expect("declared argument")
                .resolve(stack)
            {
                Ok(p) => p,
                Err(e) => return Ok(CommandResult::failure(e.to_string())),
            };
            if host.teleport_player(uuid, pos, None, None)? {
                Ok(CommandResult::message(format!(
                    "Teleported to {:.2}, {:.2}, {:.2}.",
                    pos[0], pos[1], pos[2]
                )))
            } else {
                Ok(CommandResult::failure("Teleport target is unavailable."))
            }
        }),
    )
    .unwrap();

    // /tp <targets> <location> | /tp <targets> <destination>
    let targets = b
        .add_node(
            root,
            CommandNode::argument("targets", EntityArgument::new(false, true).kind()),
        )
        .unwrap();
    b.add_node(
        targets,
        MinecraftArgument::Vec3 {
            center_correct: true,
        }
        .node("location")
        .executes(|call, host| {
            let stack = match resolve_stack(call) {
                Ok(s) => s,
                Err(r) => return Ok(r),
            };
            let players = match resolve_entities(call, host, "targets") {
                Ok(p) => p,
                Err(r) => return Ok(r),
            };
            let pos = match call
                .arguments
                .custom::<Vec3>("location")
                .expect("declared argument")
                .resolve(stack)
            {
                Ok(p) => p,
                Err(e) => return Ok(CommandResult::failure(e.to_string())),
            };
            let mut moved = 0i32;
            for player in &players {
                if host.teleport_player(player.uuid, pos, None, None)? {
                    moved += 1;
                }
            }
            if moved == 0 {
                return Ok(CommandResult::failure("No players were teleported."));
            }
            Ok(CommandResult {
                success: true,
                value: moved,
                feedback: vec![format!(
                    "Teleported {moved} player(s) to {:.2}, {:.2}, {:.2}.",
                    pos[0], pos[1], pos[2]
                )],
            })
        }),
    )
    .unwrap();
    b.add_node(
        targets,
        CommandNode::argument("destination", EntityArgument::new(true, true).kind()).executes(
            |call, host| {
                let players = match resolve_entities(call, host, "targets") {
                    Ok(p) => p,
                    Err(r) => return Ok(r),
                };
                let dest = match resolve_entities(call, host, "destination") {
                    Ok(mut d) => d.remove(0),
                    Err(r) => return Ok(r),
                };
                let mut moved = 0i32;
                for player in &players {
                    if host.teleport_player(
                        player.uuid,
                        dest.position,
                        Some(dest.rotation[0]),
                        Some(dest.rotation[1]),
                    )? {
                        moved += 1;
                    }
                }
                if moved == 0 {
                    return Ok(CommandResult::failure("No players were teleported."));
                }
                Ok(CommandResult {
                    success: true,
                    value: moved,
                    feedback: vec![format!("Teleported {moved} player(s) to {}.", dest.name)],
                })
            },
        ),
    )
    .unwrap();
}

fn register_give(b: &mut CommandRegistryBuilder) {
    let root = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("give").requires(require_op2),
        )
        .unwrap();
    let targets = b
        .add_node(
            root,
            CommandNode::argument("targets", EntityArgument::new(false, true).kind()),
        )
        .unwrap();
    let item = b
        .add_node(
            targets,
            ItemArgument
                .node("item")
                .executes(|call, host| execute_give(call, host, 1)),
        )
        .unwrap();
    b.add_node(
        item,
        CommandNode::argument("count", ArgumentKind::Integer { min: 1, max: 6400 }).executes(
            |call, host| {
                let count = call.arguments.integer("count").expect("declared argument") as u32;
                execute_give(call, host, count)
            },
        ),
    )
    .unwrap();
}

fn execute_give(
    call: &Invocation<'_>,
    host: &mut dyn CommandHost,
    count: u32,
) -> io::Result<CommandResult> {
    let players = match resolve_entities(call, host, "targets") {
        Ok(p) => p,
        Err(r) => return Ok(r),
    };
    let spec = call
        .arguments
        .custom::<ItemSpec>("item")
        .expect("declared argument");
    let item_id = match spec.resolve_existing() {
        Ok(id) => id,
        Err(e) => return Ok(CommandResult::failure(e.to_string())),
    };
    let mut given = 0i32;
    for player in &players {
        if host.give_item(player.uuid, item_id, count)? {
            given += 1;
        }
    }
    if given == 0 {
        return Ok(CommandResult::failure("No players received items."));
    }
    Ok(CommandResult {
        success: true,
        value: given,
        feedback: vec![format!(
            "Gave {count} [{}] to {given} player(s).",
            spec.name.path
        )],
    })
}

fn register_gamemode(b: &mut CommandRegistryBuilder) {
    let root = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("gamemode").requires(require_op2),
        )
        .unwrap();
    for (name, mode) in [
        ("creative", GameMode::Creative),
        ("spectator", GameMode::Spectator),
    ] {
        let self_exec: Executor = match mode {
            GameMode::Creative => {
                |call, host| execute_gamemode(call, host, GameMode::Creative, false)
            }
            GameMode::Spectator => {
                |call, host| execute_gamemode(call, host, GameMode::Spectator, false)
            }
        };
        let target_exec: Executor = match mode {
            GameMode::Creative => {
                |call, host| execute_gamemode(call, host, GameMode::Creative, true)
            }
            GameMode::Spectator => {
                |call, host| execute_gamemode(call, host, GameMode::Spectator, true)
            }
        };
        let literal = b
            .add_node(root, CommandNode::literal(name).executes(self_exec))
            .unwrap();
        b.add_node(
            literal,
            CommandNode::argument("targets", EntityArgument::new(false, true).kind())
                .executes(target_exec),
        )
        .unwrap();
    }
}

fn execute_gamemode(
    call: &Invocation<'_>,
    host: &mut dyn CommandHost,
    mode: GameMode,
    has_targets: bool,
) -> io::Result<CommandResult> {
    let label = match mode {
        GameMode::Creative => "creative",
        GameMode::Spectator => "spectator",
    };
    if !has_targets {
        let SourceKind::Player(uuid) = call.source.kind else {
            return Ok(CommandResult::failure(
                "Specify target players when changing game mode from console.",
            ));
        };
        return if host.set_game_mode(uuid, mode)? {
            Ok(CommandResult::message(format!(
                "Set own game mode to {label}."
            )))
        } else {
            Ok(CommandResult::failure("Target player is unavailable."))
        };
    }
    let players = match resolve_entities(call, host, "targets") {
        Ok(p) => p,
        Err(r) => return Ok(r),
    };
    let mut updated = 0i32;
    for player in &players {
        if host.set_game_mode(player.uuid, mode)? {
            updated += 1;
        }
    }
    if updated == 0 {
        return Ok(CommandResult::failure("No players were updated."));
    }
    Ok(CommandResult {
        success: true,
        value: updated,
        feedback: vec![format!("Set {updated} player(s) to {label} mode.")],
    })
}

fn register_op(b: &mut CommandRegistryBuilder) {
    let op = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("op").requires(require_op3),
        )
        .unwrap();
    b.add_node(
        op,
        CommandNode::argument("target", ArgumentKind::Word).executes(|call, host| {
            let target = call.arguments.text("target").expect("declared argument");
            host.set_op(target, true)
        }),
    )
    .unwrap();

    let deop = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("deop").requires(require_op3),
        )
        .unwrap();
    b.add_node(
        deop,
        CommandNode::argument("target", ArgumentKind::Word).executes(|call, host| {
            let target = call.arguments.text("target").expect("declared argument");
            host.set_op(target, false)
        }),
    )
    .unwrap();
}

fn register_kick(b: &mut CommandRegistryBuilder) {
    let root = b
        .add_node(
            NodeId::ROOT,
            CommandNode::literal("kick").requires(require_op3),
        )
        .unwrap();
    let targets = b
        .add_node(
            root,
            CommandNode::argument("targets", EntityArgument::new(false, true).kind())
                .executes(|call, host| execute_kick(call, host, "Kicked by an operator.")),
        )
        .unwrap();
    b.add_node(
        targets,
        CommandNode::argument("reason", ArgumentKind::Greedy).executes(|call, host| {
            let reason = call.arguments.text("reason").expect("declared argument");
            execute_kick(call, host, reason)
        }),
    )
    .unwrap();
}

fn execute_kick(
    call: &Invocation<'_>,
    host: &mut dyn CommandHost,
    reason: &str,
) -> io::Result<CommandResult> {
    let players = match resolve_entities(call, host, "targets") {
        Ok(p) => p,
        Err(r) => return Ok(r),
    };
    let mut kicked = 0i32;
    for player in &players {
        if host.kick_player(player.uuid, reason)? {
            kicked += 1;
        }
    }
    if kicked == 0 {
        return Ok(CommandResult::failure("No players were kicked."));
    }
    Ok(CommandResult {
        success: true,
        value: kicked,
        feedback: vec![format!("Kicked {kicked} player(s): {reason}")],
    })
}

fn register_stop(b: &mut CommandRegistryBuilder) {
    b.add_node(
        NodeId::ROOT,
        CommandNode::literal("stop")
            .requires(require_op4)
            .executes(|_, host| {
                if host.request_shutdown()? {
                    Ok(CommandResult::message("Stopping the server."))
                } else {
                    Ok(CommandResult::failure("Shutdown trigger is not attached."))
                }
            }),
    )
    .unwrap();
}
