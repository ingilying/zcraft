use crate::handler::{Session, login::LoginService, status::StatusService};

/// Shared application services, assembled by the server rather than the dispatcher.
pub struct ServerContext {
    pub status: StatusService,
    pub login: LoginService,
    pub configuration: crate::handler::configuration::ConfigurationService,
    pub play: crate::handler::play::PlayService,
}

impl ServerContext {
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
