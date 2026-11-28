//! Item usage, hotbar slot selection, and creative inventory slot updates.
use super::{InteractionCtx, blocks::apply_game_effects};
use crate::game::{Hand, ItemStack, components::EatingState, io::GameOutput};
use uuid::Uuid;

pub(super) fn handle_use_item(ctx: &mut InteractionCtx, uuid: Uuid, hand: i32, sequence: i32) {
    let Some(entity) = ctx.player_entity(uuid) else {
        return;
    };
    let effects = if let Some(hand) = Hand::from_wire(hand)
        && let Ok(p) = ctx.players.get(entity)
        && let Some(stack) = p.inv.0.hand(hand)
    {
        ctx.with_use_context(entity, |use_ctx| {
            crate::game::item_behavior(stack.item).on_use(use_ctx, hand, stack)
        })
        .unwrap_or_else(crate::game::GameEffects::pass)
    } else {
        crate::game::GameEffects::pass()
    };
    apply_game_effects(ctx, uuid, entity, effects, sequence);
}

pub(super) fn handle_select_slot(ctx: &mut InteractionCtx, uuid: Uuid, slot: u8) {
    let Some(entity) = ctx.player_entity(uuid) else {
        return;
    };
    ctx.commands.entity(entity).remove::<EatingState>();
    if let Ok(mut p) = ctx.players.get_mut(entity) {
        p.inv.0.select(slot);
    }
    ctx.emit_equipment(uuid, entity);
}

pub(super) fn handle_creative_slot(
    ctx: &mut InteractionCtx,
    uuid: Uuid,
    slot: i16,
    item: Option<Option<ItemStack>>,
) {
    let Some(entity) = ctx.player_entity(uuid) else {
        return;
    };
    let Ok(mut p) = ctx.players.get_mut(entity) else {
        return;
    };
    if !p.profile.is_creative() {
        return;
    }
    let mut equipment_dirty = false;
    if let Some(supported) = item {
        p.inv.0.set_creative(slot, supported);
        equipment_dirty = true;
    }
    let Ok(slot_idx) = usize::try_from(slot) else {
        if equipment_dirty {
            ctx.emit_equipment(uuid, entity);
        }
        return;
    };
    let slot_sync = p
        .inv
        .0
        .slots()
        .get(slot_idx)
        .map(|&s| (p.inv.0.state_id(), s));
    if equipment_dirty {
        ctx.emit_equipment(uuid, entity);
    }
    if let Some((state_id, slot_item)) = slot_sync {
        ctx.out.push(GameOutput::PlayerSlotSynced {
            uuid,
            state_id,
            slot,
            item: slot_item,
        });
    }
}
