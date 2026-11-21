//! Server-authored text chat, without player signature verification or forwarding.
use crate::{
    command::{CommandSource, valid_text},
    handler::{Context, Outcome, configuration::ConfigurationPhase},
    players::ChatRequest,
    protocol::packets::play_chat::{ChatCommand, ChatMessage, SignedChatCommand, SystemChat},
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
        let Some(profile) = &ctx.session.profile else {
            return Ok(Outcome::empty());
        };
        let mut source = CommandSource::player(profile.uuid);
        source.origin = ctx
            .session
            .play
            .as_ref()
            .map(|play| crate::command::Origin {
                world: "minecraft:overworld".into(),
                position: play.position,
            });
        let result = ctx.server.execute_command(&source, command)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
