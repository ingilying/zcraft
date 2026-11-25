//! Server-authored text chat, without player signature verification or forwarding.
use crate::{
    command::{CommandSource, Dimension, ExecutingEntity, SourceStack, valid_text},
    handler::{Context, Outcome, configuration::ConfigurationPhase},
    players::ChatRequest,
    protocol::packets::play_chat::{
        ChatCommand, ChatMessage, CommandCompletionRequest, CommandCompletionResponse,
        SignedChatCommand, SystemChat,
    },
};
use std::{io, time::Duration};
use tokio::time::Instant;

const TOKEN_INTERVAL: Duration = Duration::from_secs(1);
const BURST: u32 = 5;
const NOTICE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum Rate {
    Allow,
    Warn,
    Drop,
}

#[derive(Debug)]
pub(crate) struct ChatRate {
    credit: Duration,
    updated: Option<Instant>,
    notice: Option<Instant>,
}

impl Default for ChatRate {
    fn default() -> Self {
        Self {
            credit: TOKEN_INTERVAL * BURST,
            updated: None,
            notice: None,
        }
    }
}

impl ChatRate {
    fn consume(&mut self, now: Instant) -> Rate {
        if let Some(updated) = self.updated.replace(now) {
            self.credit = self
                .credit
                .saturating_add(now.saturating_duration_since(updated))
                .min(TOKEN_INTERVAL * BURST);
        }
        if self.credit >= TOKEN_INTERVAL {
            self.credit -= TOKEN_INTERVAL;
            Rate::Allow
        } else if self
            .notice
            .is_none_or(|notice| now.saturating_duration_since(notice) >= NOTICE_INTERVAL)
        {
            self.notice = Some(now);
            Rate::Warn
        } else {
            Rate::Drop
        }
    }
}

fn reply(text: impl Into<String>) -> Outcome {
    Outcome {
        replies: vec![SystemChat(text.into()).into()],
        ..Outcome::empty()
    }
}

fn handle(ctx: &mut Context<'_>, text: &str, is_command: bool) -> io::Result<Outcome> {
    if ctx.session.configuration != ConfigurationPhase::Finished {
        return Ok(Outcome::empty());
    }
    let Some(slot) = ctx
        .session
        .reservation
        .as_ref()
        .filter(|slot| slot.chat_ready())
    else {
        return Ok(Outcome::empty());
    };
    match ctx.session.chat_rate.consume(Instant::now()) {
        Rate::Allow => {}
        Rate::Warn => return Ok(reply("You're sending messages too quickly.")),
        Rate::Drop => return Ok(Outcome::empty()),
    }
    if !valid_text(text) {
        return Ok(reply("Chat contains disallowed characters."));
    }
    let text = text.trim();
    let command = text.strip_prefix('/').or(is_command.then_some(text));
    if let Some(command) = command {
        let Some(source) = player_stack(ctx) else {
            return Ok(Outcome::empty());
        };
        let result = ctx.server.run_command(&source, command).result;
        return Ok(Outcome {
            replies: result
                .feedback
                .into_iter()
                .map(|text| SystemChat(text).into())
                .collect(),
            ..Outcome::empty()
        });
    }
    if text.is_empty() {
        return Ok(Outcome::empty());
    }
    Ok(slot
        .chat(ChatRequest::Public(text))?
        .map_or_else(Outcome::empty, reply))
}

pub(crate) async fn handle_message(
    ctx: &mut Context<'_>,
    packet: ChatMessage,
) -> io::Result<Outcome> {
    handle(ctx, &packet.0, false)
}

pub(crate) async fn handle_command(
    ctx: &mut Context<'_>,
    packet: ChatCommand,
) -> io::Result<Outcome> {
    handle(ctx, &packet.0, true)
}

pub(crate) async fn handle_signed_command(
    ctx: &mut Context<'_>,
    packet: SignedChatCommand,
) -> io::Result<Outcome> {
    handle(ctx, &packet.0, true)
}

fn player_stack(ctx: &Context<'_>) -> Option<SourceStack> {
    let profile = ctx.session.profile.as_ref()?;
    let play = ctx.session.play.as_ref()?;
    let source = CommandSource {
        permission: ctx.server.play.permission_for(profile.uuid, &profile.name),
        ..CommandSource::player(profile.uuid)
    };
    SourceStack::new(source)
        .positioned(
            Dimension {
                id: "minecraft:overworld".into(),
                coordinate_scale: 1.0,
            },
            play.position,
        )
        .ok()?
        .with_rotation([play.yaw, play.pitch])
        .ok()?
        .with_entity(Some(ExecutingEntity {
            uuid: profile.uuid,
            eye_height: 1.62,
        }))
        .ok()
}

const COMPLETION_INTERVAL: Duration = Duration::from_millis(200);
const COMPLETION_TIMEOUT: Duration = Duration::from_millis(100);

async fn complete(
    registry: &crate::command::CommandRegistry,
    source: &CommandSource,
    input: &str,
) -> Vec<crate::command::Suggestion> {
    if input.len() > crate::command::MAX_COMMAND_BYTES || !valid_text(input) {
        return Vec::new();
    }
    // Keep the slash in the reader so replacement ranges refer to the full
    // wire input, just like Brigadier's StringReader after skip().
    let parsed = registry.dispatch_at(input, usize::from(input.starts_with('/')), source);
    tokio::time::timeout(
        COMPLETION_TIMEOUT,
        parsed.suggestions_async(input.encode_utf16().count()),
    )
    .await
    .unwrap_or_default()
}

pub(crate) async fn handle_completion(
    ctx: &mut Context<'_>,
    packet: CommandCompletionRequest,
) -> io::Result<Outcome> {
    if ctx.session.configuration != ConfigurationPhase::Finished
        || !ctx
            .session
            .reservation
            .as_ref()
            .is_some_and(|slot| slot.chat_ready())
    {
        return Ok(Outcome::empty());
    }
    let now = Instant::now();
    if ctx.session.next_completion.is_some_and(|next| now < next) {
        return Ok(Outcome::empty());
    }
    let Some(source) = player_stack(ctx) else {
        return Ok(Outcome::empty());
    };
    ctx.session.next_completion = Some(now + COMPLETION_INTERVAL);
    // The connection owns and polls this future. No lock or detached task
    // survives timeout, shutdown, or a transition out of Play.
    let matches = complete(ctx.server.play.commands(), source.source(), &packet.input).await;
    Ok(Outcome {
        replies: vec![
            CommandCompletionResponse {
                transaction: packet.transaction,
                matches,
            }
            .into(),
        ],
        ..Outcome::empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completion_timeout_drops_provider_and_reconfiguration_rejects_new_requests() {
        use crate::{
            auth::offline_profile,
            command::{
                ArgumentKind, CommandNode, CommandRegistryBuilder, NodeId, SuggestionContext,
                SuggestionFuture,
            },
            context::ServerContext,
            handler::{
                Session,
                configuration::{ConfigurationService, request_reconfiguration},
                login::LoginService,
                play::{self, PlayService},
                status::StatusService,
            },
            protocol::{Clientbound, State, packets::play::TeleportConfirm},
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn delayed(_: SuggestionContext<'_>) -> SuggestionFuture<'_> {
            Box::pin(async {
                let _guard = Guard;
                std::future::pending().await
            })
        }
        let mut builder = CommandRegistryBuilder::new();
        let node = builder
            .add_node(NodeId::ROOT, CommandNode::literal("test"))
            .unwrap();
        builder
            .add_node(
                node,
                CommandNode::argument("arg", ArgumentKind::Word).suggests_async(delayed),
            )
            .unwrap();
        let server = ServerContext {
            status: StatusService::new("test", 20),
            login: LoginService::offline(),
            configuration: ConfigurationService::new(),
            play: PlayService::new()
                .unwrap()
                .with_commands(builder.build().unwrap()),
        };
        let profile = offline_profile("Steve");
        let mut session = Session {
            state: State::Play,
            configuration: ConfigurationPhase::Finished,
            reservation: Some(server.players().reserve(profile.uuid).unwrap()),
            profile: Some(profile),
            ..Session::default()
        };
        let mut ctx = Context {
            server: &server,
            session: &mut session,
        };
        play::begin(&mut ctx).unwrap();
        ctx.session.play_initialized();
        play::handle_teleport(&mut ctx, TeleportConfirm { id: 0 })
            .await
            .unwrap();
        let request = |transaction| CommandCompletionRequest {
            transaction,
            input: "/test x".into(),
        };
        let response = handle_completion(&mut ctx, request(73)).await.unwrap();
        assert!(
            matches!(&response.replies[..], [Clientbound::CommandCompletionResponse(r)] if r.transaction == 73 && r.matches.is_empty())
        );
        assert_eq!(DROPS.load(Ordering::Relaxed), 1);
        // Make the cooldown expired so the following rejection tests phase isolation.
        ctx.session.next_completion = None;
        request_reconfiguration(&mut ctx).unwrap();
        assert!(
            handle_completion(&mut ctx, request(74))
                .await
                .unwrap()
                .replies
                .is_empty()
        );
        assert_eq!(DROPS.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn chat_and_commands_share_a_bounded_rate_and_notice_budget() {
        let now = Instant::now();
        let mut chat = ChatRate::default();
        for _ in 0..BURST {
            assert!(matches!(chat.consume(now), Rate::Allow));
        }
        assert!(matches!(chat.consume(now), Rate::Warn));
        assert!(matches!(chat.consume(now), Rate::Drop));
        assert!(matches!(chat.consume(now + TOKEN_INTERVAL), Rate::Allow));
        assert!(matches!(chat.consume(now + TOKEN_INTERVAL), Rate::Drop));
        assert!(matches!(chat.consume(now + NOTICE_INTERVAL), Rate::Allow));
        assert!(matches!(chat.consume(now + NOTICE_INTERVAL), Rate::Warn));
        let later = now + Duration::from_secs(1000);
        for _ in 0..BURST {
            assert!(matches!(chat.consume(later), Rate::Allow));
        }
        assert!(matches!(chat.consume(later), Rate::Warn));
    }

    #[test]
    fn plain_text_preserves_unicode_but_rejects_spoofing_controls() {
        assert!(valid_text("你好 مرحبا 🙂"));
        for message in ["x\ny", "x\0y", "x§by", "x\u{202e}y", "x\u{2067}y"] {
            assert!(!valid_text(message));
        }
    }
}
