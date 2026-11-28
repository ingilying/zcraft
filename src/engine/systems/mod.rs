//! Modular `bevy_ecs` systems implementing all authoritative gameplay logic.
pub(crate) mod drops;
pub(crate) mod interaction;
pub(crate) mod lifecycle;
pub(crate) mod movement;
pub(crate) mod world_tick;

use super::{
    io::GameInput,
    resources::{InputQueue, TickClock},
};
use bevy_ecs::prelude::World;

/// Process all queued inbound `GameInput`s in strict FIFO arrival order.
pub(crate) fn process_inputs_system(ecs: &mut World) {
    let inputs = std::mem::take(&mut ecs.resource_mut::<InputQueue>().0);
    let tick = ecs.resource::<TickClock>().tick;
    for input in inputs {
        match input {
            GameInput::PlayerJoin { player, inventory } => {
                lifecycle::handle_join(ecs, *player, *inventory);
            }
            GameInput::PlayerLeave { uuid, persist } => {
                lifecycle::handle_leave(ecs, uuid, persist);
            }
            GameInput::PlayerChunkTracked { uuid, chunk } => {
                lifecycle::handle_chunk_tracked(ecs, uuid, chunk);
            }
            GameInput::PlayerChunksUntracked { uuid, chunks } => {
                lifecycle::handle_chunks_untracked(ecs, uuid, chunks);
            }
            GameInput::PlayerTeleportConfirmed { uuid } => {
                lifecycle::handle_teleport_confirmed(ecs, uuid);
            }
            GameInput::PlayerRespawn { uuid } => {
                lifecycle::handle_respawn(ecs, uuid);
            }
            GameInput::ControlTeleport {
                uuid,
                position,
                yaw,
                pitch,
            } => {
                lifecycle::handle_control_teleport(ecs, uuid, position, yaw, pitch);
            }
            GameInput::ControlGive { uuid, item, count } => {
                lifecycle::handle_control_give(ecs, uuid, item, count);
            }
            GameInput::ControlSetGameMode { uuid, mode } => {
                lifecycle::handle_control_game_mode(ecs, uuid, mode);
            }
            GameInput::ControlSetBlock { pos, state, reply } => {
                lifecycle::handle_control_set_block(ecs, pos, state, reply);
            }
            GameInput::ControlFillBlocks {
                from,
                to,
                state,
                reply,
            } => {
                lifecycle::handle_control_fill_blocks(ecs, from, to, state, reply);
            }
            GameInput::ControlSetDayTime { day_time, reply } => {
                lifecycle::handle_control_set_day_time(ecs, day_time, reply);
            }
            GameInput::ControlSetWeather {
                raining,
                thundering,
                duration,
                reply,
            } => {
                lifecycle::handle_control_set_weather(ecs, raining, thundering, duration, reply);
            }
            GameInput::PlayerMove {
                uuid,
                position,
                rotation,
                on_ground,
            } => {
                movement::handle_move(ecs, uuid, position, rotation, on_ground, tick);
            }
            GameInput::PlayerEntityAction { uuid, action } => {
                movement::handle_entity_action(ecs, uuid, action);
            }
            GameInput::PlayerArmAnimation { uuid, hand } => {
                movement::handle_arm_animation(ecs, uuid, hand);
            }
            GameInput::PlayerDig {
                uuid,
                action,
                pos,
                face,
                sequence,
            } => {
                interaction::handle_dig(ecs, uuid, action, pos, face, sequence, tick);
            }
            GameInput::PlayerPlace {
                uuid,
                pos,
                face,
                hand,
                hit,
                sequence,
            } => {
                interaction::handle_place(ecs, uuid, pos, face, hand, hit, sequence);
            }
            GameInput::PlayerUseItem {
                uuid,
                hand,
                sequence,
            } => {
                interaction::handle_use_item(ecs, uuid, hand, sequence);
            }
            GameInput::PlayerSelectSlot { uuid, slot } => {
                interaction::handle_select_slot(ecs, uuid, slot);
            }
            GameInput::PlayerCreativeSlot { uuid, slot, item } => {
                interaction::handle_creative_slot(ecs, uuid, slot, item);
            }
            GameInput::PlayerInventoryClick {
                uuid,
                window,
                state_id,
                click,
            } => {
                interaction::handle_inventory_click(ecs, uuid, window, state_id, click);
            }
            GameInput::PlayerCloseWindow { uuid, window_id } => {
                interaction::handle_close_window(ecs, uuid, window_id);
            }
        }
    }
}
