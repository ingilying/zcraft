//! Configuration negotiation; packet data and services remain independent of transport.
use super::{Context, Outcome, Session};
use crate::{
    config::ResourcePackConfig,
    protocol::{Clientbound, State, invalid, packets::configuration::*},
    registry_data,
};
use std::{io, sync::Arc, time::Duration};
use tokio::time::Instant;

const PHASE_TIMEOUT: Duration = Duration::from_secs(30);
const RESOURCE_PACK_TIMEOUT: Duration = Duration::from_secs(120);
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ConfigurationService {
    registries: Vec<Arc<[u8]>>,
    tags: Arc<[u8]>,
    resource_pack: Option<AddResourcePack>,
}
impl ConfigurationService {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            registries: registry_data::registry_packets()?
                .into_iter()
                .map(Arc::from)
                .collect(),
            tags: registry_data::tags_packet()?.into(),
            resource_pack: None,
        })
    }

    /// Embedders may opt in to one vanilla pack; none is requested by default.
    pub fn with_resource_pack(mut self, pack: ResourcePackConfig) -> io::Result<Self> {
        pack.validate()?;
        self.resource_pack = Some(AddResourcePack {
            id: uuid::Uuid::parse_str(&pack.id).map_err(|_| invalid("invalid pack UUID"))?,
            url: pack.url,
            sha1: pack.sha1,
            required: pack.required,
            prompt: pack.prompt,
        });
        Ok(self)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigurationPhase {
    NotStarted,
    AwaitReconfigurationAck,
    AwaitKnownPacks,
    AwaitResourcePack,
    AwaitFinish,
    Finished,
}

#[derive(Debug)]
pub(crate) struct ConfigurationSession {
    deadline: Instant,
    next_keep_alive: Instant,
    // None timestamp means queued, not yet written.
    pending_keep_alive: Option<(i64, Option<Instant>)>,
    pack_status: Option<ResourcePackStatus>,
}
impl ConfigurationSession {
    fn new(now: Instant) -> Self {
        Self {
            deadline: now + PHASE_TIMEOUT,
            next_keep_alive: now + KEEP_ALIVE_INTERVAL,
            pending_keep_alive: None,
            pack_status: None,
        }
    }
}

/// An absolute protocol-phase deadline: heartbeat, plugin and settings traffic
/// never extend it. The request's completed write arms a fresh response window.
pub fn deadline(session: &Session) -> Option<Instant> {
    session
        .configuration_runtime
        .as_ref()
        .map(|state| state.deadline)
}

pub fn begin(ctx: &mut Context<'_>) -> io::Result<Outcome> {
    if ctx.session.state != State::Configuration
        || ctx.session.configuration != ConfigurationPhase::NotStarted
    {
        return Err(invalid("configuration already started or wrong state"));
    }
    ctx.session.configuration = ConfigurationPhase::AwaitKnownPacks;
    ctx.session.configuration_runtime = Some(ConfigurationSession::new(Instant::now()));
    Ok(Outcome {
        replies: vec![FeatureFlags.into(), SelectKnownPacks.into()],
        close: false,
        encryption: None,
    })
}

/// Public handler API for embedders: send the returned outcome through the same
/// writer as normal replies (including packet_sent), and keep decoding Play
/// until the client's Configuration Acknowledged arrives. In-flight Play packets
/// remain legal during this bounded acknowledgement window.
pub fn request_reconfiguration(ctx: &mut Context<'_>) -> io::Result<Outcome> {
    if ctx.session.state != State::Play
        || ctx.session.configuration != ConfigurationPhase::Finished
        || ctx.session.play.is_none()
        || ctx.session.profile.is_none()
    {
        return Err(invalid("cannot request reconfiguration in current state"));
    }
    if let Some(reservation) = &ctx.session.reservation {
        reservation.pause();
    }
    ctx.session.configuration = ConfigurationPhase::AwaitReconfigurationAck;
    ctx.session.configuration_runtime = Some(ConfigurationSession::new(Instant::now()));
    Ok(Outcome {
        replies: vec![StartConfiguration.into()],
        close: false,
        encryption: None,
    })
}

pub async fn handle_reconfiguration_acknowledged(
    ctx: &mut Context<'_>,
    _packet: ConfigurationAcknowledged,
) -> io::Result<Outcome> {
    if ctx.session.state != State::Play
        || ctx.session.configuration != ConfigurationPhase::AwaitReconfigurationAck
    {
        return Err(invalid("unsolicited reconfiguration acknowledgement"));
    }
    ctx.session.play = None;
    ctx.session.state = State::Configuration;
    ctx.session.configuration = ConfigurationPhase::NotStarted;
    begin(ctx)
}

pub fn packet_sent(session: &mut Session, packet: &Clientbound, now: Instant) {
    let Some(state) = session.configuration_runtime.as_mut() else {
        return;
    };
    let phase_timeout = match packet {
        Clientbound::StartConfiguration(_)
            if session.configuration == ConfigurationPhase::AwaitReconfigurationAck =>
        {
            Some(PHASE_TIMEOUT)
        }
        Clientbound::SelectKnownPacks(_)
            if session.configuration == ConfigurationPhase::AwaitKnownPacks =>
        {
            Some(PHASE_TIMEOUT)
        }
        Clientbound::AddResourcePack(_)
            if session.configuration == ConfigurationPhase::AwaitResourcePack =>
        {
            Some(RESOURCE_PACK_TIMEOUT)
        }
        Clientbound::FinishConfiguration(_)
            if session.configuration == ConfigurationPhase::AwaitFinish =>
        {
            Some(PHASE_TIMEOUT)
        }
        _ => None,
    };
    if let Some(timeout) = phase_timeout {
        state.deadline = now + timeout;
        if state.pending_keep_alive.is_none() {
            state.next_keep_alive = now + KEEP_ALIVE_INTERVAL;
        }
    }
    if let Clientbound::ConfigurationKeepAlive(packet) = packet
        && state.pending_keep_alive.map(|(id, _)| id) == Some(packet.id)
    {
        state.pending_keep_alive = Some((packet.id, Some(now)));
    }
}

pub fn disconnect(session: &Session, reason: &str) -> Outcome {
    let reply = if session.state == State::Configuration {
        ConfigurationDisconnect {
            reason: reason.into(),
        }
        .into()
    } else {
        crate::protocol::packets::play::PlayDisconnect {
            reason: reason.into(),
        }
        .into()
    };
    Outcome {
        replies: vec![reply],
        close: true,
        encryption: None,
    }
}

/// Driven by the connection timer in Configuration and while waiting for the
/// Play -> Configuration acknowledgement. No timer is refreshed by arbitrary input.
pub fn tick(session: &mut Session, now: Instant) -> io::Result<Outcome> {
    let state = session
        .configuration_runtime
        .as_mut()
        .ok_or_else(|| invalid("configuration session not initialized"))?;
    if now >= state.deadline {
        return Ok(disconnect(
            session,
            "ZCraft: configuration phase timed out.",
        ));
    }
    if session.state != State::Configuration {
        return Ok(Outcome::empty());
    }
    if state.pending_keep_alive.is_some_and(|(_, sent)| {
        sent.is_some_and(|sent| now.duration_since(sent) >= KEEP_ALIVE_TIMEOUT)
    }) {
        return Ok(disconnect(
            session,
            "ZCraft: configuration keep alive timed out.",
        ));
    }
    if state.pending_keep_alive.is_none() && now >= state.next_keep_alive {
        let id = rand::random();
        state.pending_keep_alive = Some((id, None));
        return Ok(Outcome {
            replies: vec![ConfigurationKeepAlive { id }.into()],
            close: false,
            encryption: None,
        });
    }
    Ok(Outcome::empty())
}

pub async fn handle_keep_alive(
    ctx: &mut Context<'_>,
    packet: ConfigurationKeepAliveResponse,
) -> io::Result<Outcome> {
    let state = ctx
        .session
        .configuration_runtime
        .as_mut()
        .ok_or_else(|| invalid("configuration session not initialized"))?;
    if !state
        .pending_keep_alive
        .is_some_and(|(id, sent)| id == packet.id && sent.is_some())
    {
        return Err(invalid("unexpected configuration keep alive response"));
    }
    let now = Instant::now();
    if state
        .pending_keep_alive
        .is_some_and(|(_, sent)| sent.is_some_and(|sent| now >= sent + KEEP_ALIVE_TIMEOUT))
    {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "configuration keep alive response was late",
        ));
    }
    state.pending_keep_alive = None;
    state.next_keep_alive = now + KEEP_ALIVE_INTERVAL;
    Ok(Outcome::empty())
}

pub async fn handle_information(
    ctx: &mut Context<'_>,
    packet: ConfigurationClientInformation,
) -> io::Result<Outcome> {
    ctx.session.client_information = Some(packet.0);
    Ok(Outcome::empty())
}
pub async fn handle_plugin(
    _ctx: &mut Context<'_>,
    _packet: ConfigurationPluginMessage,
) -> io::Result<Outcome> {
    // Vanilla brand/plugin channels are optional; no gameplay effects.
    Ok(Outcome::empty())
}
pub async fn handle_known_packs(
    ctx: &mut Context<'_>,
    _packet: KnownPacksResponse,
) -> io::Result<Outcome> {
    if ctx.session.configuration != ConfigurationPhase::AwaitKnownPacks {
        return Err(invalid("unexpected known-pack response"));
    }
    let mut replies = Vec::with_capacity(ctx.server.configuration.registries.len() + 2);
    for body in &ctx.server.configuration.registries {
        replies.push(RegistryData(body.clone()).into());
    }
    replies.push(UpdateTags(ctx.server.configuration.tags.clone()).into());
    if let Some(pack) = &ctx.server.configuration.resource_pack
        && !ctx
            .session
            .loaded_resource_pack
            .as_ref()
            .is_some_and(|(id, sha1)| *id == pack.id && sha1.eq_ignore_ascii_case(&pack.sha1))
    {
        // Packs survive a configuration round trip. Re-offering the same UUID
        // would replace it and can produce late Discarded/Accepted responses.
        ctx.session.configuration = ConfigurationPhase::AwaitResourcePack;
        replies.push(pack.clone().into());
    } else {
        ctx.session.configuration = ConfigurationPhase::AwaitFinish;
        replies.push(FinishConfiguration.into());
    }
    Ok(Outcome {
        replies,
        close: false,
        encryption: None,
    })
}

pub async fn handle_resource_pack(
    ctx: &mut Context<'_>,
    packet: ResourcePackResponse,
) -> io::Result<Outcome> {
    use ResourcePackStatus::*;
    if ctx.session.configuration != ConfigurationPhase::AwaitResourcePack {
        return Err(invalid("unexpected resource pack response"));
    }
    let pack = ctx
        .server
        .configuration
        .resource_pack
        .as_ref()
        .ok_or_else(|| invalid("no resource pack was offered"))?;
    if packet.id != pack.id {
        return Err(invalid("resource pack UUID mismatch"));
    }
    let state = ctx
        .session
        .configuration_runtime
        .as_mut()
        .ok_or_else(|| invalid("configuration session not initialized"))?;
    match packet.status {
        Accepted if state.pack_status.is_none() => {
            state.pack_status = Some(Accepted);
            return Ok(Outcome::empty());
        }
        Downloaded if state.pack_status == Some(Accepted) => {
            state.pack_status = Some(Downloaded);
            return Ok(Outcome::empty());
        }
        SuccessfullyLoaded if state.pack_status == Some(Downloaded) => {
            ctx.session.loaded_resource_pack = Some((pack.id, pack.sha1.clone()));
        }
        Declined | FailedDownload | InvalidUrl | FailedReload | Discarded => {
            if pack.required {
                return Ok(disconnect(
                    ctx.session,
                    "ZCraft: the required resource pack could not be loaded.",
                ));
            }
        }
        _ => return Err(invalid("invalid resource pack status sequence")),
    }
    ctx.session.configuration = ConfigurationPhase::AwaitFinish;
    Ok(Outcome {
        replies: vec![FinishConfiguration.into()],
        close: false,
        encryption: None,
    })
}

pub async fn handle_finish(
    ctx: &mut Context<'_>,
    _packet: FinishConfigurationAcknowledged,
) -> io::Result<Outcome> {
    if ctx.session.configuration != ConfigurationPhase::AwaitFinish || ctx.session.profile.is_none()
    {
        return Err(invalid("unexpected configuration acknowledgement"));
    }
    ctx.session.configuration = ConfigurationPhase::Finished;
    ctx.session.configuration_runtime = None;
    ctx.session.state = State::Play;
    super::play::begin(ctx)
}
