use std::time::Duration;
use tokio::time::Instant;
use zcraft::{
    auth::offline_profile,
    context::{Context, ServerContext},
    handler::{
        Dispatcher, Session,
        configuration::{ConfigurationPhase, ConfigurationService},
        login::{LoginPhase, LoginService},
        play::PlayService,
        status::StatusService,
    },
    protocol::{
        Clientbound, Decode, Reader, State, decode,
        packets::{
            common::ClientInformation,
            configuration::{
                ConfigurationAcknowledged, ConfigurationKeepAliveResponse,
                FinishConfigurationAcknowledged, KnownPacksResponse, ResourcePackResponse,
                ResourcePackStatus,
            },
            login::LoginAcknowledged,
            play::{KeepAliveResponse, TeleportConfirm},
        },
    },
};

use zcraft::config::ResourcePackConfig;
use zcraft::handler::configuration::{self, request_reconfiguration};

fn pack(required: bool) -> ResourcePackConfig {
    ResourcePackConfig {
        id: "00000000-0000-0000-0000-000000000001".into(),
        url: "https://example.org/pack.zip".into(),
        sha1: "0123456789012345678901234567890123456789".into(),
        required,
        prompt: Some("Load the test pack?".into()),
    }
}

fn services() -> ServerContext {
    ServerContext {
        status: StatusService::new("test", 20),
        login: LoginService::offline(),
        configuration: ConfigurationService::new(),
        play: PlayService::new().unwrap(),
    }
}
fn logged_in() -> Session {
    let mut session = Session::default();
    session.state = State::Login;
    session.login = LoginPhase::AwaitAcknowledged;
    session.profile = Some(offline_profile("Steve"));
    session
}

#[tokio::test]
async fn configuration_rejects_early_finish_and_duplicate_known_packs() {
    let server = services();
    let dispatcher = Dispatcher;
    let mut session = logged_in();
    let mut ctx = Context {
        session: &mut session,
        server: &server,
    };
    let outcome = dispatcher
        .dispatch(&mut ctx, LoginAcknowledged.into())
        .await
        .unwrap();
    assert_eq!(ctx.session.state, State::Configuration);
    assert_eq!(
        ctx.session.configuration,
        ConfigurationPhase::AwaitKnownPacks
    );
    assert!(matches!(
        outcome.replies.as_slice(),
        [
            Clientbound::FeatureFlags(_),
            Clientbound::SelectKnownPacks(_)
        ]
    ));
    assert!(
        dispatcher
            .dispatch(&mut ctx, FinishConfigurationAcknowledged.into())
            .await
            .is_err()
    );
    let outcome = dispatcher
        .dispatch(&mut ctx, KnownPacksResponse.into())
        .await
        .unwrap();
    assert_eq!(outcome.replies.len(), 13);
    assert!(matches!(
        outcome.replies.last(),
        Some(Clientbound::FinishConfiguration(_))
    ));
    assert!(
        dispatcher
            .dispatch(&mut ctx, KnownPacksResponse.into())
            .await
            .is_err()
    );
    let outcome = dispatcher
        .dispatch(&mut ctx, FinishConfigurationAcknowledged.into())
        .await
        .unwrap();
    assert_eq!(ctx.session.state, State::Play);
    assert!(matches!(
        outcome.replies.first(),
        Some(Clientbound::JoinGame(_))
    ));
    assert!(matches!(
        outcome.replies.last(),
        Some(Clientbound::PlayerPosition(_))
    ));
    assert_eq!(
        outcome
            .replies
            .iter()
            .filter(|p| matches!(p, Clientbound::ChunkData(_)))
            .count(),
        9
    );
    assert!(
        dispatcher
            .dispatch(&mut ctx, FinishConfigurationAcknowledged.into())
            .await
            .is_err()
    );
    // Unsolicited or wrong confirmations cannot reset liveness timers.
    assert!(
        dispatcher
            .dispatch(&mut ctx, TeleportConfirm { id: 1 }.into())
            .await
            .is_err()
    );
    dispatcher
        .dispatch(&mut ctx, TeleportConfirm { id: 0 }.into())
        .await
        .unwrap();
    assert!(
        dispatcher
            .dispatch(&mut ctx, TeleportConfirm { id: 0 }.into())
            .await
            .is_err()
    );
    assert!(
        dispatcher
            .dispatch(&mut ctx, KeepAliveResponse { id: 42 }.into())
            .await
            .is_err()
    );
}

async fn start_configuration(session: &mut Session, server: &ServerContext) {
    let outcome = Dispatcher
        .dispatch(&mut Context { session, server }, LoginAcknowledged.into())
        .await
        .unwrap();
    for reply in &outcome.replies {
        configuration::packet_sent(session, reply, Instant::now());
    }
}

async fn offer_pack(session: &mut Session, server: &ServerContext) {
    start_configuration(session, server).await;
    let outcome = Dispatcher
        .dispatch(&mut Context { session, server }, KnownPacksResponse.into())
        .await
        .unwrap();
    assert!(matches!(
        outcome.replies.last(),
        Some(Clientbound::AddResourcePack(_))
    ));
    assert!(
        !outcome
            .replies
            .iter()
            .any(|reply| matches!(reply, Clientbound::FinishConfiguration(_)))
    );
    for reply in &outcome.replies {
        configuration::packet_sent(session, reply, Instant::now());
    }
}

async fn pack_response(
    session: &mut Session,
    server: &ServerContext,
    status: ResourcePackStatus,
) -> std::io::Result<zcraft::handler::Outcome> {
    Dispatcher
        .dispatch(
            &mut Context { session, server },
            ResourcePackResponse {
                id: uuid::Uuid::parse_str(&pack(false).id).unwrap(),
                status,
            }
            .into(),
        )
        .await
}

#[tokio::test]
async fn resource_pack_success_and_optional_failure_gate_finish() {
    use ResourcePackStatus::*;
    let mut server = services();
    server.configuration = server
        .configuration
        .with_resource_pack(pack(false))
        .unwrap();
    for terminal in [
        SuccessfullyLoaded,
        Declined,
        FailedDownload,
        InvalidUrl,
        FailedReload,
        Discarded,
    ] {
        let mut session = logged_in();
        offer_pack(&mut session, &server).await;
        assert!(
            Dispatcher
                .dispatch(
                    &mut Context {
                        session: &mut session,
                        server: &server
                    },
                    FinishConfigurationAcknowledged.into()
                )
                .await
                .is_err()
        );
        // Unknown UUIDs and out-of-order/duplicate statuses do not advance state.
        assert!(
            Dispatcher
                .dispatch(
                    &mut Context {
                        session: &mut session,
                        server: &server
                    },
                    ResourcePackResponse {
                        id: uuid::Uuid::nil(),
                        status: Accepted
                    }
                    .into()
                )
                .await
                .is_err()
        );
        assert!(
            pack_response(&mut session, &server, Downloaded)
                .await
                .is_err()
        );
        assert!(
            pack_response(&mut session, &server, SuccessfullyLoaded)
                .await
                .is_err()
        );
        if terminal == SuccessfullyLoaded {
            assert!(
                pack_response(&mut session, &server, Accepted)
                    .await
                    .unwrap()
                    .replies
                    .is_empty()
            );
            assert!(
                pack_response(&mut session, &server, Accepted)
                    .await
                    .is_err()
            );
            assert!(
                pack_response(&mut session, &server, Downloaded)
                    .await
                    .unwrap()
                    .replies
                    .is_empty()
            );
        }
        let outcome = pack_response(&mut session, &server, terminal)
            .await
            .unwrap();
        assert!(!outcome.close);
        assert!(matches!(
            outcome.replies.as_slice(),
            [Clientbound::FinishConfiguration(_)]
        ));
        assert!(
            pack_response(&mut session, &server, terminal)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn required_resource_pack_failure_disconnects_and_success_continues() {
    use ResourcePackStatus::*;
    let mut server = services();
    server.configuration = server.configuration.with_resource_pack(pack(true)).unwrap();
    for status in [
        Declined,
        FailedDownload,
        InvalidUrl,
        FailedReload,
        Discarded,
    ] {
        let mut session = logged_in();
        offer_pack(&mut session, &server).await;
        let outcome = pack_response(&mut session, &server, status).await.unwrap();
        assert!(outcome.close);
        assert!(matches!(
            outcome.replies.as_slice(),
            [Clientbound::ConfigurationDisconnect(_)]
        ));
    }
    let mut session = logged_in();
    offer_pack(&mut session, &server).await;
    for status in [Accepted, Downloaded, SuccessfullyLoaded] {
        assert!(
            !pack_response(&mut session, &server, status)
                .await
                .unwrap()
                .close
        );
    }
    assert_eq!(session.configuration, ConfigurationPhase::AwaitFinish);
}

#[tokio::test]
async fn configuration_deadlines_start_at_flush_and_are_not_refreshed_by_heartbeats() {
    let mut server = services();
    server.configuration = server
        .configuration
        .with_resource_pack(pack(false))
        .unwrap();
    let mut session = logged_in();
    offer_pack(&mut session, &server).await;
    let phase_deadline = configuration::deadline(&session).unwrap();
    let now = Instant::now();
    let outcome = configuration::tick(&mut session, now + Duration::from_secs(10)).unwrap();
    let Clientbound::ConfigurationKeepAlive(request) = &outcome.replies[0] else {
        panic!("missing heartbeat")
    };
    let id = request.id;
    // A response to a queued-but-unsent or unknown heartbeat is rejected.
    assert!(
        Dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server
                },
                ConfigurationKeepAliveResponse { id }.into()
            )
            .await
            .is_err()
    );
    let sent = now + Duration::from_secs(20);
    configuration::packet_sent(&mut session, &outcome.replies[0], sent);
    assert!(
        !configuration::tick(&mut session, sent + Duration::from_secs(29))
            .unwrap()
            .close
    );
    assert!(
        configuration::tick(&mut session, sent + Duration::from_secs(30))
            .unwrap()
            .close
    );
    assert!(
        Dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server
                },
                ConfigurationKeepAliveResponse {
                    id: id.wrapping_add(1)
                }
                .into()
            )
            .await
            .is_err()
    );
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            ConfigurationKeepAliveResponse { id }.into(),
        )
        .await
        .unwrap();
    assert_eq!(configuration::deadline(&session), Some(phase_deadline));
    assert!(
        Dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server
                },
                ConfigurationKeepAliveResponse { id }.into()
            )
            .await
            .is_err()
    );
    // Resource progress also cannot perpetually refresh the phase window.
    pack_response(&mut session, &server, ResourcePackStatus::Accepted)
        .await
        .unwrap();
    assert_eq!(configuration::deadline(&session), Some(phase_deadline));
    assert!(
        configuration::tick(&mut session, phase_deadline)
            .unwrap()
            .close
    );
}

#[tokio::test]
async fn slow_registry_batch_preserves_the_finish_acknowledgement_window() {
    let server = services();
    let mut session = logged_in();
    start_configuration(&mut session, &server).await;
    let outcome = Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            KnownPacksResponse.into(),
        )
        .await
        .unwrap();
    // Simulate a response batch delayed 25 seconds by output backpressure.
    let sent = Instant::now() + Duration::from_secs(25);
    for reply in &outcome.replies {
        configuration::packet_sent(&mut session, reply, sent);
    }
    assert_eq!(
        configuration::deadline(&session),
        Some(sent + Duration::from_secs(30))
    );
    assert!(
        !configuration::tick(&mut session, sent + Duration::from_secs(29))
            .unwrap()
            .close
    );
    assert!(
        configuration::tick(&mut session, sent + Duration::from_secs(30))
            .unwrap()
            .close
    );
}

#[tokio::test]
async fn public_reconfiguration_api_requires_ack_then_repeats_configuration_and_play() {
    let server = services();
    let mut session = Session::default();
    session.state = State::Login;
    let mut login = vec![0];
    zcraft::protocol::put_string("Alice", &mut login).unwrap();
    login.extend_from_slice(uuid::Uuid::nil().as_bytes());
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            decode(State::Login, &login).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        request_reconfiguration(&mut Context {
            session: &mut session,
            server: &server
        })
        .is_err()
    );
    start_configuration(&mut session, &server).await;
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            KnownPacksResponse.into(),
        )
        .await
        .unwrap();
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            FinishConfigurationAcknowledged.into(),
        )
        .await
        .unwrap();
    assert!(
        Dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server
                },
                ConfigurationAcknowledged.into()
            )
            .await
            .is_err()
    );
    let packet = zcraft::protocol::packets::play::TimeUpdate::default().into();
    assert_eq!(server.players().online_count(), 0);
    assert_eq!(server.players().broadcast(&packet).unwrap().queued, 0);
    session.play_initialized();
    assert_eq!(server.players().online_count(), 1);
    assert_eq!(server.players().broadcast(&packet).unwrap().queued, 1);
    let profile = session.profile.clone().unwrap();
    let entity_id = session.play.as_ref().unwrap().entity_id;
    let outcome = request_reconfiguration(&mut Context {
        session: &mut session,
        server: &server,
    })
    .unwrap();
    assert!(matches!(
        outcome.replies.as_slice(),
        [Clientbound::StartConfiguration(_)]
    ));
    assert_eq!(outcome.replies[0].encode().unwrap(), [0x69]);
    assert_eq!(session.state, State::Play);
    session.play_initialized(); // Cannot unpause during the acknowledgement window.
    assert_eq!(server.players().online_count(), 1);
    assert_eq!(server.players().broadcast(&packet).unwrap().queued, 0);
    assert!(
        request_reconfiguration(&mut Context {
            session: &mut session,
            server: &server
        })
        .is_err()
    );
    let sent = Instant::now() + Duration::from_secs(20);
    configuration::packet_sent(&mut session, &outcome.replies[0], sent);
    assert!(
        !configuration::tick(&mut session, sent + Duration::from_secs(29))
            .unwrap()
            .close
    );
    let expired = configuration::tick(&mut session, sent + Duration::from_secs(30)).unwrap();
    assert!(expired.close);
    assert!(matches!(
        expired.replies.as_slice(),
        [Clientbound::PlayDisconnect(_)]
    ));
    let outcome = Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            decode(State::Play, &[0x0c]).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.replies.last(),
        Some(Clientbound::SelectKnownPacks(_))
    ));
    assert_eq!(session.state, State::Configuration);
    assert!(session.play.is_none());
    assert_eq!(session.profile.as_ref().unwrap().uuid, profile.uuid);
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            KnownPacksResponse.into(),
        )
        .await
        .unwrap();
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            FinishConfigurationAcknowledged.into(),
        )
        .await
        .unwrap();
    assert_eq!(session.state, State::Play);
    assert_eq!(session.play.as_ref().unwrap().entity_id, entity_id);
    assert!(configuration::deadline(&session).is_none());
    assert_eq!(server.players().online_count(), 1);
    assert_eq!(server.players().broadcast(&packet).unwrap().queued, 0);
    session.play_initialized();
    assert_eq!(server.players().online_count(), 1);
    assert_eq!(server.players().broadcast(&packet).unwrap().queued, 1);
    drop(session);
    assert_eq!(server.players().online_count(), 0);
}

#[tokio::test]
async fn reconfiguration_retains_an_already_loaded_resource_pack() {
    let mut server = services();
    server.configuration = server.configuration.with_resource_pack(pack(true)).unwrap();
    let mut session = logged_in();
    offer_pack(&mut session, &server).await;
    for status in [
        ResourcePackStatus::Accepted,
        ResourcePackStatus::Downloaded,
        ResourcePackStatus::SuccessfullyLoaded,
    ] {
        pack_response(&mut session, &server, status).await.unwrap();
    }
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            FinishConfigurationAcknowledged.into(),
        )
        .await
        .unwrap();
    request_reconfiguration(&mut Context {
        session: &mut session,
        server: &server,
    })
    .unwrap();
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            ConfigurationAcknowledged.into(),
        )
        .await
        .unwrap();
    let outcome = Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            KnownPacksResponse.into(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.replies.last(),
        Some(Clientbound::FinishConfiguration(_))
    ));
    assert!(
        !outcome
            .replies
            .iter()
            .any(|reply| matches!(reply, Clientbound::AddResourcePack(_)))
    );
    Dispatcher
        .dispatch(
            &mut Context {
                session: &mut session,
                server: &server,
            },
            FinishConfigurationAcknowledged.into(),
        )
        .await
        .unwrap();
    assert_eq!(session.state, State::Play);
}

#[test]
fn resource_pack_and_heartbeat_wire_layouts_are_strict() {
    use zcraft::protocol::packets::configuration::{AddResourcePack, ConfigurationKeepAlive};
    let config = pack(true);
    let id = uuid::Uuid::parse_str(&config.id).unwrap();
    let encoded = Clientbound::AddResourcePack(AddResourcePack {
        id,
        url: config.url.clone(),
        sha1: config.sha1.clone(),
        required: true,
        prompt: config.prompt.clone(),
    })
    .encode()
    .unwrap();
    let mut reader = Reader::new(&encoded);
    assert_eq!(reader.varint().unwrap(), 9);
    assert_eq!(reader.take(16).unwrap(), id.as_bytes());
    assert_eq!(reader.string(32767).unwrap(), config.url);
    assert_eq!(reader.string(40).unwrap(), config.sha1);
    assert_eq!(reader.take(2).unwrap(), [1, 1]);
    assert_eq!(reader.take(1).unwrap(), [8]); // anonymous NBT string component
    let heartbeat = Clientbound::ConfigurationKeepAlive(ConfigurationKeepAlive { id: 42 })
        .encode()
        .unwrap();
    assert_eq!(heartbeat, [vec![4], 42i64.to_be_bytes().to_vec()].concat());
    assert!(decode(State::Configuration, &heartbeat).is_ok());
    for bytes in [&[4][..], &[4, 1], &[4, 0, 0, 0, 0, 0, 0, 0, 0, 0]] {
        assert!(decode(State::Configuration, bytes).is_err());
    }
    let mut response = vec![6];
    response.extend_from_slice(id.as_bytes());
    for status in 0..=7 {
        let mut bytes = response.clone();
        bytes.push(status);
        assert!(decode(State::Configuration, &bytes).is_ok());
        bytes.push(0);
        assert!(decode(State::Configuration, &bytes).is_err());
    }
    response.push(8);
    assert!(decode(State::Configuration, &response).is_err());
    assert!(decode(State::Play, &[0x0c, 0]).is_err());
}

#[test]
fn resource_pack_prompt_uses_java_modified_utf8() {
    use zcraft::protocol::packets::configuration::AddResourcePack;
    let encoded = Clientbound::AddResourcePack(AddResourcePack {
        id: uuid::Uuid::nil(),
        url: "https://example.org/pack.zip".into(),
        sha1: "0".repeat(40),
        required: false,
        prompt: Some("中\0😀".into()),
    })
    .encode()
    .unwrap();
    assert!(encoded.ends_with(&[
        8, 0, 11, // TAG_String, eleven modified-UTF8 bytes
        0xe4, 0xb8, 0xad, 0xc0, 0x80, 0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80,
    ]));
}

#[test]
fn resource_pack_configuration_is_opt_in_and_validated() {
    assert!(zcraft::config::Config::default().resource_pack.is_none());
    assert!(pack(false).validate().is_ok());
    for (field, value) in [
        ("id", "bad-uuid"),
        ("url", "file:///pack.zip"),
        ("url", "https://"),
        ("sha1", "not-a-sha1"),
    ] {
        let mut config = pack(false);
        match field {
            "id" => config.id = value.into(),
            "url" => config.url = value.into(),
            _ => config.sha1 = value.into(),
        }
        assert!(config.validate().is_err());
    }
    let mut config = pack(false);
    config.prompt = Some("😀".repeat(11000));
    assert!(config.validate().is_err());
    let value = zcraft::config::Config {
        resource_pack: Some(pack(true)),
        ..Default::default()
    };
    let text = toml::to_string(&value).unwrap();
    assert_eq!(
        toml::from_str::<zcraft::config::Config>(&text).unwrap(),
        value
    );
}

#[test]
fn known_pack_selection_must_match_the_empty_offer() {
    assert!(decode(State::Configuration, &[7, 0]).is_ok());
    assert!(decode(State::Configuration, &[7, 1]).is_err());
    assert!(decode(State::Configuration, &[7, 0, 0]).is_err());
    assert!(decode(State::Configuration, &[3, 0]).is_err());
}

#[test]
fn client_information_is_bounded_and_strictly_decoded() {
    let data = [5, b'e', b'n', b'_', b'u', b's', 8, 0, 1, 127, 1, 0, 1];
    let mut reader = Reader::new(&data);
    let info = ClientInformation::decode(&mut reader).unwrap();
    assert_eq!(info.locale, "en_us");
    assert_eq!(info.view_distance, 8);
    reader.finish().unwrap();
    for (index, value) in [(6, 255), (7, 3), (8, 2), (10, 2), (11, 2), (12, 2)] {
        let mut invalid = data;
        invalid[index] = value;
        assert!(ClientInformation::decode(&mut Reader::new(&invalid)).is_err());
    }
    for len in 0..data.len() {
        assert!(ClientInformation::decode(&mut Reader::new(&data[..len])).is_err());
    }
}

#[tokio::test]
async fn slow_initial_batch_does_not_consume_the_teleport_confirmation_window() {
    let server = services();
    let dispatcher = Dispatcher;
    let mut session = logged_in();
    let mut ctx = Context {
        session: &mut session,
        server: &server,
    };
    dispatcher
        .dispatch(&mut ctx, LoginAcknowledged.into())
        .await
        .unwrap();
    dispatcher
        .dispatch(&mut ctx, KnownPacksResponse.into())
        .await
        .unwrap();
    let outcome = dispatcher
        .dispatch(&mut ctx, FinishConfigurationAcknowledged.into())
        .await
        .unwrap();
    let position = outcome.replies.last().unwrap();
    assert!(matches!(position, Clientbound::PlayerPosition(_)));
    // Simulate 20 seconds of backpressure before the final position packet is
    // flushed. The confirmation deadline must start at flush, not begin().
    let flushed = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    zcraft::handler::play::packet_sent(
        ctx.session,
        &ctx.server.play,
        ctx.server.players(),
        position,
        flushed,
    )
    .unwrap();
    assert!(
        !zcraft::handler::play::tick(ctx.session, flushed + std::time::Duration::from_secs(14))
            .unwrap()
            .close
    );
    assert!(
        zcraft::handler::play::tick(ctx.session, flushed + std::time::Duration::from_secs(15))
            .unwrap()
            .close
    );
}
