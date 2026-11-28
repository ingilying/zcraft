//! Authoritative ECS gameplay logic for block digging/placing, item use,
//! hotbar selection, creative slots, containers (Chest, Crafting Table, Furnace),
//! and player inventory clicks.
mod blocks;
mod containers;
mod items;

pub(crate) use blocks::{handle_dig, handle_place};
pub(crate) use containers::{handle_close_window, handle_inventory_click};
pub(crate) use items::{handle_creative_slot, handle_select_slot, handle_use_item};

use super::drops::queue_drop;
use crate::game::{
    Hand, ItemStack,
    components::{
        NetworkEntityId, PlayerContainer, PlayerInventory, PlayerProfile, Position, Rotation,
        player_eye_position,
    },
    events::InteractionMsg,
    io::GameOutput,
    resources::{OutputQueue, TickClock},
};
use bevy_ecs::prelude::{Entity, Messages, World};
use uuid::Uuid;

/// Consume queued `InteractionMsg` events for digging, placing, item use,
/// hotbar selection, creative slots, and container/inventory clicks.
pub(crate) fn player_interaction_system(ecs: &mut World) {
    let msgs: Vec<InteractionMsg> = ecs
        .resource_mut::<Messages<InteractionMsg>>()
        .drain()
        .collect();
    let tick = ecs.resource::<TickClock>().tick;
    for msg in msgs {
        match msg {
            InteractionMsg::Dig {
                uuid,
                action,
                pos,
                face,
                sequence,
            } => handle_dig(ecs, uuid, action, pos, face, sequence, tick),
            InteractionMsg::Place {
                uuid,
                pos,
                face,
                hand,
                hit,
                sequence,
            } => handle_place(ecs, uuid, pos, face, hand, hit, sequence),
            InteractionMsg::UseItem {
                uuid,
                hand,
                sequence,
            } => handle_use_item(ecs, uuid, hand, sequence),
            InteractionMsg::SelectSlot { uuid, slot } => handle_select_slot(ecs, uuid, slot),
            InteractionMsg::CreativeSlot { uuid, slot, item } => {
                handle_creative_slot(ecs, uuid, slot, item)
            }
            InteractionMsg::InventoryClick {
                uuid,
                window,
                state_id,
                click,
            } => handle_inventory_click(ecs, uuid, window, state_id, click),
            InteractionMsg::CloseWindow { uuid, window_id } => {
                handle_close_window(ecs, uuid, window_id)
            }
        }
    }
}

pub(super) fn emit_equipment(ecs: &mut World, uuid: Uuid, entity: Entity) {
    let Some(entity_id) = ecs.get::<NetworkEntityId>(entity).map(|id| id.0) else {
        return;
    };
    let Some((main_hand, off_hand)) = ecs
        .get::<PlayerInventory>(entity)
        .map(|i| (i.0.hand(Hand::Main), i.0.hand(Hand::Off)))
    else {
        return;
    };
    ecs.resource_mut::<OutputQueue>()
        .push(GameOutput::PlayerEquipmentChanged {
            uuid,
            entity_id,
            main_hand,
            off_hand,
        });
}

pub(super) fn emit_inventory_if_can_interact(ecs: &mut World, uuid: Uuid, entity: Entity) {
    if !ecs
        .get::<PlayerProfile>(entity)
        .is_some_and(PlayerProfile::can_interact)
    {
        return;
    }
    if let Some(inv) = ecs.get::<PlayerInventory>(entity).map(|i| i.0.clone()) {
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerInventorySynced {
                uuid,
                inventory: inv,
            });
    }
}

pub(super) fn throw_from_player(ecs: &mut World, entity: Entity, stack: ItemStack) {
    let (Some(&pos), Some(&rot)) = (ecs.get::<Position>(entity), ecs.get::<Rotation>(entity))
    else {
        return;
    };
    let eye = player_eye_position(pos);
    let yaw_rad = f64::from(rot.yaw).to_radians();
    let pitch_rad = f64::from(rot.pitch).to_radians();
    let vx = -yaw_rad.sin() * pitch_rad.cos() * 0.3;
    let vy = -pitch_rad.sin() * 0.3 + 0.1;
    let vz = yaw_rad.cos() * pitch_rad.cos() * 0.3;
    queue_drop(ecs, eye, [vx, vy, vz], stack, 40);
}

pub(super) fn allocate_window_id(ecs: &mut World, entity: Entity) -> u8 {
    let mut c = ecs.get_mut::<PlayerContainer>(entity).unwrap();
    let id = c.next_window_id;
    c.next_window_id = if id >= 100 { 1 } else { id + 1 };
    id
}
