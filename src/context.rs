use crate::handler::{Session, login::LoginService, status::StatusService};

/// Shared application services, assembled by the server rather than the dispatcher.
pub struct ServerContext {
    pub status: StatusService,
    pub login: LoginService,
}

/// Per-dispatch borrows. Handlers have no access to the transport.
pub struct Context<'a> {
    pub session: &'a mut Session,
    pub server: &'a ServerContext,
}
