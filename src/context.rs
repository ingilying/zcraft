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
        self.play
            .commands()
            .execute(input, source, &mut self.players())
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
