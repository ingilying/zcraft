mod admin;
mod world;
use super::{arguments::*, *};

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

    world::register_time(&mut builder);
    world::register_weather(&mut builder);
    world::register_setblock(&mut builder);
    world::register_fill(&mut builder);
    admin::register_tp(&mut builder);
    admin::register_give(&mut builder);
    admin::register_gamemode(&mut builder);
    admin::register_op(&mut builder);
    admin::register_kick(&mut builder);
    admin::register_stop(&mut builder);
    builder
}

pub(super) fn require_op2(source: &CommandSource) -> bool {
    source.permission >= 2
}
pub(super) fn require_op3(source: &CommandSource) -> bool {
    source.permission >= 3
}
pub(super) fn require_op4(source: &CommandSource) -> bool {
    source.permission >= 4
}

pub(super) fn resolve_stack<'a>(
    call: &'a Invocation<'_>,
) -> Result<&'a SourceStack, CommandResult> {
    call.stack
        .ok_or_else(|| CommandResult::failure("Command runtime context is required."))
}

pub(super) fn world_host(host: &mut dyn CommandHost) -> io::Result<&mut dyn WorldCommandHost> {
    host.world()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "World host is unavailable."))
}

pub(super) fn admin_host(host: &mut dyn CommandHost) -> io::Result<&mut dyn AdminCommandHost> {
    host.admin()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "Admin host is unavailable."))
}

pub(super) fn resolve_entities(
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
