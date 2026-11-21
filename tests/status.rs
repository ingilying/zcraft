use zcraft::{
    context::{Context, ServerContext},
    handler::{Dispatcher, Session, status::StatusService},
    protocol::{Clientbound, State, packets::status::StatusRequest},
};

#[test]
fn status_service_builds_response_without_dispatcher() {
    let service = StatusService::new("Hello \"世界\"", 42);
    let first = service.response();
    let json: serde_json::Value = serde_json::from_str(&first.json).unwrap();
    assert_eq!(json["description"]["text"], "Hello \"世界\"");
    assert_eq!(json["players"]["max"], 42);
    assert_eq!(json["players"]["online"], 0);
    assert_eq!(json["version"]["protocol"], 767);
    assert_eq!(service.response(), first);
}

#[tokio::test]
async fn dispatcher_uses_caller_services_without_retaining_status() {
    let dispatcher = Dispatcher;
    for motd in ["server A", "server B"] {
        let server = ServerContext {
            status: StatusService::new(motd, 20),
            login: zcraft::handler::login::LoginService::offline(),
            configuration: zcraft::handler::configuration::ConfigurationService::new(),
            play: zcraft::handler::play::PlayService::new().unwrap(),
        };
        let mut session = Session::default();
        session.state = State::Status;
        let result = dispatcher
            .dispatch(
                &mut Context {
                    session: &mut session,
                    server: &server,
                },
                StatusRequest.into(),
            )
            .await
            .unwrap();
        let Some(Clientbound::Status(response)) = result.replies.into_iter().next() else {
            panic!("expected status response")
        };
        let json: serde_json::Value = serde_json::from_str(&response.json).unwrap();
        assert_eq!(json["description"]["text"], motd);
    }
}
