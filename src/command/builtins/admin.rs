use super::{admin_host, require_op2, require_op3, require_op4, resolve_entities, resolve_stack};
use crate::{
    command::{arguments::*, *},
    config::GameMode,
};

pub(super) fn register_tp(b: &mut CommandRegistryBuilder) {
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
            if admin_host(host)?.teleport_player(uuid, pos, None, None)? {
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
                if admin_host(host)?.teleport_player(player.uuid, pos, None, None)? {
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
                    if admin_host(host)?.teleport_player(
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

pub(super) fn register_give(b: &mut CommandRegistryBuilder) {
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
        if admin_host(host)?.give_item(player.uuid, item_id, count)? {
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

pub(super) fn register_gamemode(b: &mut CommandRegistryBuilder) {
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
        return if admin_host(host)?.set_game_mode(uuid, mode)? {
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
        if admin_host(host)?.set_game_mode(player.uuid, mode)? {
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

pub(super) fn register_op(b: &mut CommandRegistryBuilder) {
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
            admin_host(host)?.set_op(target, true)
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
            admin_host(host)?.set_op(target, false)
        }),
    )
    .unwrap();
}

pub(super) fn register_kick(b: &mut CommandRegistryBuilder) {
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
        if admin_host(host)?.kick_player(player.uuid, reason)? {
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

pub(super) fn register_stop(b: &mut CommandRegistryBuilder) {
    b.add_node(
        NodeId::ROOT,
        CommandNode::literal("stop")
            .requires(require_op4)
            .executes(|_, host| {
                if admin_host(host)?.request_shutdown()? {
                    Ok(CommandResult::message("Stopping the server."))
                } else {
                    Ok(CommandResult::failure("Shutdown trigger is not attached."))
                }
            }),
    )
    .unwrap();
}
