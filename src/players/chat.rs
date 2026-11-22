use super::{Delivery, Registry, Reservation, Slot, lock};
use crate::{
    command::{CommandHost, CommandResult},
    protocol::{Clientbound, packets::play_chat::SystemChat},
};
use std::{collections::BTreeSet, io, sync::Arc};
use uuid::Uuid;

pub(crate) enum ChatRequest<'a> {
    Public(&'a str),
    Action(&'a str),
    Whisper { target: &'a str, message: &'a str },
}

impl Slot {
    fn chat_ready(&self) -> bool {
        self.receiving && !self.lagged && self.visible.is_some()
    }
}

impl Registry {
    fn enqueue_whisper(&mut self, uuid: Uuid, body: Arc<[u8]>) -> bool {
        let delivered = self
            .players
            .get_mut(&uuid)
            .expect("known recipient")
            .enqueue(Delivery::Chat {
                body,
                private: true,
            });
        if !delivered {
            self.deactivate(uuid);
        }
        delivered
    }

    fn chat_roster(&self) -> CommandResult {
        const DISPLAY_LIMIT: usize = 32;
        let mut count: usize = 0;
        let mut first_names = BTreeSet::new();
        for slot in self.players.values().filter(|slot| slot.chat_ready()) {
            let peer = slot.visible.as_ref().expect("ready peer");
            count += 1;
            first_names.insert((peer.name.as_ref(), peer.uuid));
            if first_names.len() > DISPLAY_LIMIT {
                first_names.pop_last();
            }
        }
        let names = first_names
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
            .join(", ");
        let remaining = count.saturating_sub(DISPLAY_LIMIT);
        let text = if remaining == 0 {
            format!("Online ({count}): {names}")
        } else {
            format!("Online ({count}): {names} and {remaining} more")
        };
        CommandResult {
            success: true,
            value: count.min(i32::MAX as usize) as i32,
            feedback: vec![text],
        }
    }
}

impl Reservation {
    pub(crate) fn set_chat_mode(&self, mode: i32) {
        if let Some(slot) = self.0.slot(&mut lock(&self.0.registry)) {
            slot.chat_mode = mode;
            if mode != 0 {
                slot.queue.retain(|delivery| match delivery {
                    Delivery::Chat { private, .. } => *private && mode == 1,
                    _ => true,
                });
            }
        }
    }

    pub(crate) fn chat_ready(&self) -> bool {
        self.0
            .slot(&mut lock(&self.0.registry))
            .is_some_and(|slot| slot.chat_ready())
    }

    /// Queue text only for eligible Play sessions, returning private feedback.
    pub(crate) fn chat(&self, request: ChatRequest<'_>) -> io::Result<Option<String>> {
        let mut registry = lock(&self.0.registry);
        if !self
            .0
            .slot(&mut registry)
            .is_some_and(|slot| slot.chat_ready())
        {
            return Ok(None);
        }
        Ok(registry
            .dispatch_chat(self.0.uuid, request)?
            .feedback
            .into_iter()
            .next())
    }
}

impl Registry {
    fn dispatch_chat(
        &mut self,
        sender_uuid: Uuid,
        request: ChatRequest<'_>,
    ) -> io::Result<CommandResult> {
        let Some(sender) = self
            .players
            .get_mut(&sender_uuid)
            .filter(|slot| slot.chat_ready())
        else {
            return Ok(CommandResult::failure("Player unavailable."));
        };
        let mode = sender.chat_mode;
        let name = sender.visible.as_ref().expect("ready sender").name.clone();
        let unavailable = || CommandResult::failure("Player unavailable.");
        match request {
            ChatRequest::Public(message) | ChatRequest::Action(message) => {
                if mode != 0 {
                    return Ok(CommandResult::failure(
                        "Public chat is disabled in your chat settings.",
                    ));
                }
                let text = match request {
                    ChatRequest::Action(_) => format!("* {name} {message}"),
                    _ => format!("<{name}> {message}"),
                };
                let body: Arc<[u8]> = Clientbound::from(SystemChat(text)).encode()?.into();
                let mut lagged = Vec::new();
                for (&uuid, slot) in &mut self.players {
                    if slot.chat_ready()
                        && slot.chat_mode == 0
                        && !slot.enqueue(Delivery::Chat {
                            body: body.clone(),
                            private: false,
                        })
                    {
                        lagged.push(uuid);
                    }
                }
                for uuid in lagged {
                    self.deactivate(uuid);
                }
                Ok(CommandResult::success(1))
            }
            ChatRequest::Whisper { target, message } => {
                if mode == 2 {
                    return Ok(CommandResult::failure(
                        "Private chat is disabled in your chat settings.",
                    ));
                }
                let mut candidate = None;
                let mut ambiguous = false;
                for slot in self.players.values().filter(|slot| slot.chat_ready()) {
                    let peer = slot.visible.as_ref().expect("ready peer");
                    if &*peer.name == target {
                        candidate = Some((peer, slot.chat_mode));
                        ambiguous = false;
                        break;
                    }
                    if peer.name.eq_ignore_ascii_case(target) {
                        ambiguous |= candidate.replace((peer, slot.chat_mode)).is_some();
                    }
                }
                let Some((peer, target_mode)) = candidate.filter(|_| !ambiguous) else {
                    return Ok(unavailable());
                };
                if target_mode == 2 {
                    return Ok(unavailable());
                }
                let recipient = peer.uuid;
                let target_name = peer.name.clone();
                if recipient != sender_uuid {
                    let body = Clientbound::from(SystemChat(format!("[From {name}] {message}")))
                        .encode()?
                        .into();
                    if !self.enqueue_whisper(recipient, body) {
                        return Ok(unavailable());
                    }
                }
                Ok(CommandResult::message(format!(
                    "[To {target_name}] {message}"
                )))
            }
        }
    }
}

/// Host bound to the originating player's current ready-Play lifetime. A
/// reconnect or reconfiguration must not revive an older queued command chain.
struct SessionCommandHost<'a> {
    players: &'a super::Players,
    binding: Option<(Uuid, Option<Arc<()>>)>,
}

impl super::Players {
    pub fn command_host(&self, source: &crate::command::SourceStack) -> impl CommandHost + '_ {
        let binding = source.authenticated_player().map(|uuid| {
            let epoch = lock(&self.registry)
                .players
                .get(&uuid)
                .filter(|slot| slot.chat_ready())
                .map(|slot| slot.command_epoch.clone());
            (uuid, epoch)
        });
        SessionCommandHost {
            players: self,
            binding,
        }
    }
}

impl SessionCommandHost<'_> {
    fn active(&self, registry: &Registry) -> bool {
        self.binding.as_ref().is_none_or(|(uuid, epoch)| {
            epoch.as_ref().is_some_and(|epoch| {
                registry.players.get(uuid).is_some_and(|slot| {
                    slot.chat_ready() && Arc::ptr_eq(epoch, &slot.command_epoch)
                })
            })
        })
    }
    fn player_chat(&self, player: Uuid, request: ChatRequest<'_>) -> io::Result<CommandResult> {
        let mut registry = lock(&self.players.registry);
        if self.binding.as_ref().map(|(uuid, _)| *uuid) != Some(player) || !self.active(&registry) {
            return Ok(CommandResult::failure("Player unavailable."));
        }
        registry.dispatch_chat(player, request)
    }
}

impl CommandHost for SessionCommandHost<'_> {
    fn command_active(&self, source: &crate::command::SourceStack) -> bool {
        source.authenticated_player() == self.binding.as_ref().map(|(uuid, _)| *uuid)
            && self.active(&lock(&self.players.registry))
    }
    fn list_players(&mut self) -> io::Result<CommandResult> {
        Ok(lock(&self.players.registry).chat_roster())
    }
    fn action(&mut self, player: Uuid, text: &str) -> io::Result<CommandResult> {
        if !safe_command_text(text) {
            return Ok(CommandResult::failure("Invalid command text."));
        }
        self.player_chat(player, ChatRequest::Action(text))
    }
    fn whisper(&mut self, player: Uuid, target: &str, text: &str) -> io::Result<CommandResult> {
        if !crate::auth::valid_username(target) || !safe_command_text(text) {
            return Ok(CommandResult::failure("Invalid command text."));
        }
        self.player_chat(
            player,
            ChatRequest::Whisper {
                target,
                message: text,
            },
        )
    }
}

impl CommandHost for &super::Players {
    fn command_active(&self, source: &crate::command::SourceStack) -> bool {
        source.authenticated_player().is_none_or(|player| {
            lock(&self.registry)
                .players
                .get(&player)
                .is_some_and(Slot::chat_ready)
        })
    }
    fn list_players(&mut self) -> io::Result<CommandResult> {
        Ok(lock(&self.registry).chat_roster())
    }
    fn action(&mut self, player: Uuid, text: &str) -> io::Result<CommandResult> {
        if !safe_command_text(text) {
            return Ok(CommandResult::failure("Invalid command text."));
        }
        lock(&self.registry).dispatch_chat(player, ChatRequest::Action(text))
    }
    fn whisper(&mut self, player: Uuid, target: &str, text: &str) -> io::Result<CommandResult> {
        if !crate::auth::valid_username(target) || !safe_command_text(text) {
            return Ok(CommandResult::failure("Invalid command text."));
        }
        lock(&self.registry).dispatch_chat(
            player,
            ChatRequest::Whisper {
                target,
                message: text,
            },
        )
    }
}

fn safe_command_text(text: &str) -> bool {
    text.len() <= crate::command::MAX_COMMAND_BYTES && crate::command::valid_text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        players::{Peer, Players},
        protocol::packets::play_entities::EntityPose,
    };

    fn ready(players: &Players, id: u128, name: &str) -> Reservation {
        let uuid = Uuid::from_u128(id);
        let slot = players.reserve(uuid).unwrap();
        slot.enter_play();
        slot.show(Peer {
            uuid,
            name: name.into(),
            id: id as i32,
            pose: EntityPose::new([0.0; 3], 0.0, 0.0, false),
            info: Arc::from([]),
        });
        slot
    }

    fn queued(slot: &Reservation) -> usize {
        slot.0
            .slot(&mut lock(&slot.0.registry))
            .unwrap()
            .queue
            .len()
    }

    #[test]
    fn whisper_prefers_exact_case_and_never_delivers_ambiguous_names() {
        let players = Players::new(4);
        let alice = ready(&players, 1, "Alice");
        let bob = ready(&players, 2, "Bob");
        let alternate = ready(&players, 3, "BOB");
        let whisper = |target| {
            alice
                .chat(ChatRequest::Whisper {
                    target,
                    message: "secret",
                })
                .unwrap()
                .unwrap()
        };
        assert_eq!(whisper("bob"), "Player unavailable.");
        assert_eq!((queued(&bob), queued(&alternate)), (0, 0));
        assert_eq!(whisper("Bob"), "[To Bob] secret");
        assert_eq!((queued(&bob), queued(&alternate)), (1, 0));
        bob.pause();
        assert_eq!(queued(&bob), 0);
        assert_eq!(whisper("bob"), "[To BOB] secret");
        assert_eq!(queued(&alternate), 1);
    }

    #[test]
    fn command_host_does_not_revive_after_reconfiguration_or_reconnect() {
        use crate::command::{CommandSource, SourceStack};
        let players = Players::new(1);
        let alice = ready(&players, 1, "Alice");
        let source = SourceStack::new(CommandSource::player(Uuid::from_u128(1)));
        let mut old_host = players.command_host(&source);
        assert!(old_host.command_active(&source));
        alice.pause();
        assert!(!old_host.command_active(&source));
        alice.enter_play();
        alice.show(Peer {
            uuid: Uuid::from_u128(1),
            name: "Alice".into(),
            id: 1,
            pose: EntityPose::new([8.5, 65.0, 8.5], 0.0, 0.0, false),
            info: Arc::from([]),
        });
        assert!(!old_host.command_active(&source));
        assert!(
            !old_host
                .action(Uuid::from_u128(1), "stale")
                .unwrap()
                .success
        );
        let second_host = players.command_host(&source);
        assert!(second_host.command_active(&source));
        drop(alice);
        let _replacement = ready(&players, 1, "Alice");
        assert!(!old_host.command_active(&source));
        assert!(!second_host.command_active(&source));
        assert!(players.command_host(&source).command_active(&source));
    }

    #[test]
    fn settings_and_phase_clear_pending_chat_and_skip_unready_players() {
        let players = Players::new(3);
        let alice = ready(&players, 1, "Alice");
        let bob = ready(&players, 2, "Bob");
        let pending = players.reserve(Uuid::from_u128(3)).unwrap();
        pending.enter_play();
        alice.chat(ChatRequest::Public("hello")).unwrap();
        assert_eq!(queued(&bob), 1);
        assert_eq!(queued(&pending), 0);
        bob.set_chat_mode(1);
        assert_eq!(queued(&bob), 0);
        let whisper = || {
            alice
                .chat(ChatRequest::Whisper {
                    target: "Bob",
                    message: "secret",
                })
                .unwrap()
                .unwrap()
        };
        assert_eq!(whisper(), "[To Bob] secret");
        assert_eq!(queued(&bob), 1);
        bob.set_chat_mode(2);
        assert_eq!(queued(&bob), 0);
        assert_eq!(whisper(), "Player unavailable.");
        bob.pause();
        assert_eq!(bob.chat(ChatRequest::Public("not in Play")).unwrap(), None);
        assert_eq!(whisper(), "Player unavailable.");
        assert_eq!(
            (&players).list_players().unwrap().feedback,
            ["Online (1): Alice"]
        );
    }
}
