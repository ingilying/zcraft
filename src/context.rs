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
            &mut self.players().command_host(source),
            crate::command::ExecutionLimits::default(),
        )
    }

    /// Shared admission, presence, and broadcast service.
    pub fn players(&self) -> &crate::players::Players {
        self.status.players()
    }
}

/// Per-dispatch borrows. Handlers have no access to the transport.
pub struct Context<'a> {
    pub session: &'a mut Session,
    pub server: &'a ServerContext,
}
