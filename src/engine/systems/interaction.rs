//! Authoritative ECS gameplay logic for block digging/placing, item use,
//! hotbar selection, creative slots, containers (Chest, Crafting Table, Furnace),
//! and player inventory clicks.
mod blocks;
mod containers;
mod items;

pub(crate) use blocks::{handle_dig, handle_place};
pub(crate) use containers::{
    handle_close_window, handle_inventory_click, refresh_container_viewers,
};
pub(crate) use items::{handle_creative_slot, handle_select_slot, handle_use_item};

use super::drops::spawn_drop;
use crate::{
    engine::{
        components::{PlayerContainer, PlayerData, PlayerInventory},
        io::GameOutput,
        resources::OutputQueue,
    },
    game::{Hand, ItemStack},
};
use bevy_ecs::prelude::{Entity, World};
use uuid::Uuid;

pub(super) fn emit_equipment(ecs: &mut World, uuid: Uuid, entity: Entity) {
    let Some(entity_id) = ecs.get::<PlayerData>(entity).map(|p| p.0.entity_id) else {
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
        .get::<PlayerData>(entity)
        .is_some_and(|p| p.0.can_interact())
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
    let Some((eye, yaw, pitch)) = ecs
        .get::<PlayerData>(entity)
        .map(|p| (p.0.eye_position(), p.0.yaw, p.0.pitch))
    else {
        return;
    };
    let yaw_rad = f64::from(yaw).to_radians();
    let pitch_rad = f64::from(pitch).to_radians();
    let vx = -yaw_rad.sin() * pitch_rad.cos() * 0.3;
    let vy = -pitch_rad.sin() * 0.3 + 0.1;
    let vz = yaw_rad.cos() * pitch_rad.cos() * 0.3;
    spawn_drop(ecs, eye, [vx, vy, vz], stack, 40);
}

pub(super) fn allocate_window_id(ecs: &mut World, entity: Entity) -> u8 {
    let mut c = ecs.get_mut::<PlayerContainer>(entity).unwrap();
    let id = c.next_window_id;
    c.next_window_id = if id >= 100 { 1 } else { id + 1 };
    id
}
