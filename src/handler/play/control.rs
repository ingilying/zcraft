//! Server-initiated Play control actions (teleport, give, gamemode, OP refresh, kick).
use super::{Outcome, PlayService, Session, closed};
use crate::{
    config::GameMode,
    game::Player,
    players::PlayerControl,
    protocol::{
        invalid,
        packets::{
            play::{GameEvent, PlayerAbilities, PlayerPosition},
            play_blocks::{InventoryContent, SelectedSlot},
            play_chat::DeclareCommands,
        },
    },
};
use std::io;
use tokio::time::Instant;

pub(crate) fn apply_control(
    session: &mut Session,
    service: &PlayService,
    control: PlayerControl,
) -> io::Result<Outcome> {
    let slot = session.reservation.as_ref();
    let state = session
        .play
        .as_mut()
        .ok_or_else(|| invalid("Play session not initialized"))?;
    match control {
        PlayerControl::Teleport {
            position,
            yaw,
            pitch,
        } => {
            if !Player::position_in_bounds(position) {
                return Ok(Outcome::empty());
            }
            let id = state.next_teleport;
            state.next_teleport = id
                .checked_add(1)
                .ok_or_else(|| invalid("teleport ID exhausted"))?;
            state.pending_teleport = Some((id, Instant::now()));
            state.player.position = position;
            if let Some(y) = yaw {
                state.player.yaw = y;
            }
            if let Some(p) = pitch {
                state.player.pitch = p;
            }
            state.player.on_ground = false;
            let old_view = state.chunks.view();
            state.chunks.follow(position[0], position[2]);
            let yaw = state.player.yaw;
            let pitch = state.player.pitch;
            if let Some(slot) = slot
                && old_view != state.chunks.view()
            {
                state.update_view(slot);
            }
            Ok(Outcome {
                replies: vec![PlayerPosition::at(position, yaw, pitch, id).into()],
                close: false,
                encryption: None,
            })
        }
        PlayerControl::Give { item, count } => {
            session.inventory.give(item, count);
            Ok(Outcome {
                replies: vec![InventoryContent(Box::new(session.inventory.clone())).into()],
                close: false,
                encryption: None,
            })
        }
        PlayerControl::SetGameMode(mode) => {
            state.player.game_mode = mode;
            if let Some(slot) = slot {
                slot.update_game_mode(mode);
            }
            let mut replies = vec![
                GameEvent::change_game_mode(mode).into(),
                PlayerAbilities(mode).into(),
            ];
            if mode == GameMode::Creative {
                replies.push(InventoryContent(Box::new(session.inventory.clone())).into());
                replies.push(SelectedSlot(session.inventory.selected()).into());
            }
            Ok(Outcome {
                replies,
                close: false,
                encryption: None,
            })
        }
        PlayerControl::RefreshCommands => {
            let source = crate::command::CommandSource {
                permission: service.permission_for(state.player.uuid, &state.player.name),
                ..crate::command::CommandSource::player(state.player.uuid)
            };
            Ok(Outcome {
                replies: vec![DeclareCommands::new(service.commands(), &source).into()],
                close: false,
                encryption: None,
            })
        }
        PlayerControl::Kick(reason) => Ok(closed(&reason)),
    }
}
