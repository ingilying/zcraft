use crate::handler::{Session, status::StatusService};

/// Shared application services, assembled by the server rather than the dispatcher.
pub struct ServerContext {
    pub status: StatusService,
}

/// Per-dispatch borrows. Handlers have no access to the transport.
pub struct Context<'a> {
    pub session: &'a mut Session,
    pub server: &'a ServerContext,
}
