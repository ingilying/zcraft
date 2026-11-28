//! Item usage, hotbar slot selection, and creative inventory slot updates.
use super::{blocks::apply_game_effects, emit_equipment};
use crate::{
    engine::{
        components::{
            EatingState, PlayerGameMode, PlayerInventory, apply_player_dto, snapshot_player_dto,
        },
        io::GameOutput,
        resources::{EntityRegistry, OutputQueue, VoxelWorld},
    },
    game::{Hand, ItemStack},
};
use bevy_ecs::prelude::World;
use uuid::Uuid;

pub(crate) fn handle_use_item(ecs: &mut World, uuid: Uuid, hand: i32, sequence: i32) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let tick = ecs.resource::<crate::engine::resources::TickClock>().tick;
    let effects = if let Some(hand) = Hand::from_wire(hand)
        && let Some(stack) = ecs
            .get::<PlayerInventory>(entity)
            .and_then(|i| i.0.hand(hand))
        && let Some(mut player) = snapshot_player_dto(ecs, entity)
    {
        let mut inv = ecs.get::<PlayerInventory>(entity).unwrap().0.clone();
        let mut use_ctx = crate::game::UseContext {
            world: &world,
            player: &mut player,
            inventory: &mut inv,
            tick,
        };
        let fx = crate::game::item_behavior(stack.item).on_use(&mut use_ctx, hand, stack);
        apply_player_dto(ecs, entity, &player);
        ecs.get_mut::<PlayerInventory>(entity).unwrap().0 = inv;
        fx
    } else {
        crate::game::GameEffects::pass()
    };
    apply_game_effects(ecs, uuid, entity, effects, sequence);
}

pub(crate) fn handle_select_slot(ecs: &mut World, uuid: Uuid, slot: u8) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if let Ok(mut e) = ecs.get_entity_mut(entity) {
        e.remove::<EatingState>();
    }
    if let Some(mut inv) = ecs.get_mut::<PlayerInventory>(entity) {
        inv.0.select(slot);
    }
    emit_equipment(ecs, uuid, entity);
}

pub(crate) fn handle_creative_slot(
    ecs: &mut World,
    uuid: Uuid,
    slot: i16,
    item: Option<Option<ItemStack>>,
) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if !ecs
        .get::<PlayerGameMode>(entity)
        .is_some_and(|gm| gm.is_creative())
    {
        return;
    }
    if let Some(supported) = item {
        if let Some(mut inv) = ecs.get_mut::<PlayerInventory>(entity) {
            inv.0.set_creative(slot, supported);
        }
        emit_equipment(ecs, uuid, entity);
    }
    let Ok(slot_idx) = usize::try_from(slot) else {
        return;
    };
    let Some((state_id, slot_item)) = ecs
        .get::<PlayerInventory>(entity)
        .and_then(|inv| inv.0.slots().get(slot_idx).map(|&s| (inv.0.state_id(), s)))
    else {
        return;
    };
    ecs.resource_mut::<OutputQueue>()
        .push(GameOutput::PlayerSlotSynced {
            uuid,
            state_id,
            slot,
            item: slot_item,
        });
}
