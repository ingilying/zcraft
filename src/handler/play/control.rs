//! Server-initiated Play control actions (teleport, give, gamemode, OP refresh, kick).
use super::{Outcome, PlayService, Session, closed};
use crate::{
    config::GameMode,
    game::Player,
    players::PlayerControl,
    protocol::{
        invalid,
        packets::{
            play::{GameEvent, PlayerAbilities, PlayerPosition, UpdateHealth},
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
    players: &crate::players::Players,
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
                replies: vec![InventoryContent::player(&session.inventory).into()],
                close: false,
                encryption: None,
            })
        }
        PlayerControl::TryPickup(drop_id) => {
            if state.player.dead || !state.player.can_interact() {
                return Ok(Outcome::empty());
            }
            let player_id = state.player.entity_id;
            if service.try_pickup_drop(players, drop_id, player_id, &mut session.inventory) {
                Ok(Outcome {
                    replies: vec![InventoryContent::player(&session.inventory).into()],
                    close: false,
                    encryption: None,
                })
            } else {
                Ok(Outcome::empty())
            }
        }
        PlayerControl::RefreshContainer(pos) => {
            let Some(open) = state.open_container.clone() else {
                return Ok(Outcome::empty());
            };
            match open.kind {
                super::blocks::ContainerKind::Chest(cpos) if cpos == pos => {
                    let Some(chest_items) = service.world().chest_items(pos) else {
                        state.open_container = None;
                        return Ok(Outcome {
                            replies: vec![
                                crate::protocol::packets::play_blocks::CloseContainer(
                                    open.window_id,
                                )
                                .into(),
                            ],
                            close: false,
                            encryption: None,
                        });
                    };
                    let next_state_id = open.state_id.wrapping_add(1);
                    state.open_container = Some(super::blocks::OpenContainer {
                        state_id: next_state_id,
                        ..open
                    });
                    let mut slots = Vec::with_capacity(crate::world::CHEST_SLOTS + 36);
                    slots.extend_from_slice(&chest_items);
                    slots.extend_from_slice(&session.inventory.slots()[9..45]);
                    Ok(Outcome {
                        replies: vec![
                            InventoryContent::container(
                                open.window_id,
                                next_state_id,
                                slots,
                                session.inventory.carried(),
                            )
                            .into(),
                        ],
                        close: false,
                        encryption: None,
                    })
                }
                super::blocks::ContainerKind::Furnace(fpos) if fpos == pos => {
                    let Some(furnace) = service.world().furnace_data(pos) else {
                        state.open_container = None;
                        return Ok(Outcome {
                            replies: vec![
                                crate::protocol::packets::play_blocks::CloseContainer(
                                    open.window_id,
                                )
                                .into(),
                            ],
                            close: false,
                            encryption: None,
                        });
                    };
                    let next_state_id = open.state_id.wrapping_add(1);
                    state.open_container = Some(super::blocks::OpenContainer {
                        state_id: next_state_id,
                        ..open
                    });
                    let mut slots = Vec::with_capacity(39);
                    slots.extend_from_slice(&furnace.slots());
                    slots.extend_from_slice(&session.inventory.slots()[9..45]);
                    let mut replies = vec![
                        InventoryContent::container(
                            open.window_id,
                            next_state_id,
                            slots,
                            session.inventory.carried(),
                        )
                        .into(),
                    ];
                    replies.extend(super::blocks::furnace_property_packets(
                        open.window_id,
                        &furnace,
                    ));
                    Ok(Outcome {
                        replies,
                        close: false,
                        encryption: None,
                    })
                }
                _ => Ok(Outcome::empty()),
            }
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
            if mode == GameMode::Survival {
                replies.push(
                    UpdateHealth::new(
                        state.player.health,
                        state.player.food,
                        state.player.saturation,
                    )
                    .into(),
                );
            }
            if mode != GameMode::Spectator {
                replies.push(InventoryContent::player(&session.inventory).into());
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
