use super::{require_op2, resolve_stack, world_host};
use crate::command::{arguments::*, *};

pub(super) fn register_time(b: &mut CommandRegistryBuilder) {
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
            let (_, day_time) = world_host(host)?.world_time()?;
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
            let (world_age, _) = world_host(host)?.world_time()?;
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
            let (_, day_time) = world_host(host)?.world_time()?;
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
            let (_, current) = world_host(host)?.world_time()?;
            set_time_result(host, current.saturating_add(delta))
        }),
    )
    .unwrap();
}

fn set_time_result(host: &mut dyn CommandHost, target: i64) -> io::Result<CommandResult> {
    let time = world_host(host)?.set_time(target)?;
    Ok(CommandResult {
        success: true,
        value: time as i32,
        feedback: vec![format!("Set the time to {time}.")],
    })
}

pub(super) fn register_weather(b: &mut CommandRegistryBuilder) {
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
    world_host(host)?.set_weather(raining, thundering, ticks)?;
    let label = match (raining, thundering) {
        (false, _) => "clear",
        (true, false) => "rain",
        (true, true) => "rain & thunder",
    };
    Ok(CommandResult::message(format!(
        "Set the weather to {label}."
    )))
}

pub(super) fn register_setblock(b: &mut CommandRegistryBuilder) {
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
            if world_host(host)?.set_block(pos, state)? {
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

pub(super) fn register_fill(b: &mut CommandRegistryBuilder) {
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
            match world_host(host)?.fill_blocks(from, to, state) {
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
