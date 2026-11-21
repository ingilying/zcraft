use super::*;

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
    builder
}
