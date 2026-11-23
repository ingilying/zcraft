use crate::handler::{Session, login::LoginService, status::StatusService};

/// Shared application services, assembled by the server rather than the dispatcher.
pub struct ServerContext {
    pub status: StatusService,
    pub login: LoginService,
    pub configuration: crate::handler::configuration::ConfigurationService,
    pub play: crate::handler::play::PlayService,
}

impl ServerContext {
    /// Execute a trusted server command without a player connection. Permission
    /// and source are assigned by the caller, never taken from a client packet.
    pub fn execute_command(
        &self,
        source: &crate::command::CommandSource,
        input: &str,
    ) -> std::io::Result<crate::command::CommandResult> {
        Ok(self
            .run_command(&crate::command::SourceStack::new(source.clone()), input)
            .result)
    }

    /// Run one bounded Minecraft command chain with a trusted source stack.
    pub fn run_command(
        &self,
        source: &crate::command::SourceStack,
        input: &str,
    ) -> crate::command::ExecutionReport {
        self.play.commands().run(
            input,
            source,
            &mut ServerCommandHost {
                server: self,
                player: self.players().command_host(source),
            },
            crate::command::ExecutionLimits::default(),
        )
    }

    /// Snapshot currently ready entities for command evaluation. The current
    /// world contains players only; returned values do not retain registry locks.
    pub fn command_entities(&self) -> Vec<crate::command::arguments::EntitySnapshot> {
        self.players().command_entities(self.play.game_mode())
    }

    /// Shared admission, presence, and broadcast service.
    pub fn players(&self) -> &crate::players::Players {
        self.status.players()
    }
}

struct ServerCommandHost<'a, H> {
    server: &'a ServerContext,
    player: H,
}

impl<H: crate::command::CommandHost> crate::command::CommandHost for ServerCommandHost<'_, H> {
    fn list_players(&mut self) -> std::io::Result<crate::command::CommandResult> {
        self.player.list_players()
    }
    fn action(
        &mut self,
        player: uuid::Uuid,
        text: &str,
    ) -> std::io::Result<crate::command::CommandResult> {
        self.player.action(player, text)
    }
    fn whisper(
        &mut self,
        player: uuid::Uuid,
        target: &str,
        text: &str,
    ) -> std::io::Result<crate::command::CommandResult> {
        self.player.whisper(player, target, text)
    }
    fn command_active(&self, source: &crate::command::SourceStack) -> bool {
        self.player.command_active(source)
    }
    fn command_entities(
        &mut self,
    ) -> std::io::Result<Vec<crate::command::arguments::EntitySnapshot>> {
        Ok(self.server.command_entities())
    }
}

/// Per-dispatch borrows. Handlers have no access to the transport.
pub struct Context<'a> {
    pub session: &'a mut Session,
    pub server: &'a ServerContext,
}
