//! Container window lifecycle, multi-viewer sync, and inventory/container clicks.
use super::InteractionCtx;
use crate::{
    game::{
        InventoryClick, ItemStack,
        events::SpawnDropMsg,
        io::{ContainerKind, GameOutput, OpenContainer},
    },
    world::BlockPos,
};
use uuid::Uuid;

pub(super) fn refresh_container_viewers(ctx: &mut InteractionCtx, pos: BlockPos, exclude: Uuid) {
    let others = ctx.viewers.others(pos, exclude);
    if others.is_empty() {
        return;
    }
    let chest_items = ctx.world.0.chest_items(pos);
    let furnace_data = ctx.world.0.furnace_data(pos);

    for viewer_uuid in others {
        let Some(entity) = ctx.player_entity(viewer_uuid) else {
            continue;
        };
        let Ok((_, _, _, _, _, _, _, _, _, _, _, inv, mut container, _)) =
            ctx.players.get_mut(entity)
        else {
            continue;
        };
        let Some(open) = container.open.clone() else {
            continue;
        };
        match open.kind {
            ContainerKind::Chest(cpos) if cpos == pos => {
                if let Some(items) = &chest_items {
                    let next_state_id = open.state_id.wrapping_add(1);
                    container.open = Some(OpenContainer {
                        state_id: next_state_id,
                        ..open
                    });
                    let slots = crate::game::containers::container_slots(items, &inv.0);
                    ctx.out.push(GameOutput::ContainerSynced {
                        uuid: viewer_uuid,
                        window_id: open.window_id,
                        state_id: next_state_id,
                        slots,
                        carried: inv.0.carried(),
                        furnace_props: None,
                    });
                } else {
                    container.open = None;
                    ctx.out.push(GameOutput::ContainerClosed {
                        uuid: viewer_uuid,
                        window_id: open.window_id,
                    });
                }
            }
            ContainerKind::Furnace(fpos) if fpos == pos => {
                if let Some(furnace) = &furnace_data {
                    let next_state_id = open.state_id.wrapping_add(1);
                    container.open = Some(OpenContainer {
                        state_id: next_state_id,
                        ..open
                    });
                    let slots = crate::game::containers::furnace_slots(furnace, &inv.0);
                    let props = [
                        furnace.burn_time as i16,
                        furnace.burn_total as i16,
                        furnace.cook_time as i16,
                        furnace.cook_total as i16,
                    ];
                    ctx.out.push(GameOutput::ContainerSynced {
                        uuid: viewer_uuid,
                        window_id: open.window_id,
                        state_id: next_state_id,
                        slots,
                        carried: inv.0.carried(),
                        furnace_props: Some(props),
                    });
                } else {
                    container.open = None;
                    ctx.out.push(GameOutput::ContainerClosed {
                        uuid: viewer_uuid,
                        window_id: open.window_id,
                    });
                }
            }
            _ => {}
        }
    }
}

pub(super) fn handle_close_window(ctx: &mut InteractionCtx, uuid: Uuid, window_id: u8) {
    let Some(entity) = ctx.player_entity(uuid) else {
        return;
    };
    let (pos, closed, mut returned) = {
        let Ok((_, _, _, pos, _, _, _, _, _, _, _, mut inv, mut container, _)) =
            ctx.players.get_mut(entity)
        else {
            return;
        };
        let closed = if let Some(open) = &container.open
            && (window_id == 0 || open.window_id == window_id)
        {
            container.open.take()
        } else {
            None
        };
        let mut returned = Vec::new();
        if window_id == 0 {
            returned.extend(inv.0.drain_crafting_grid());
        }
        if let Some(carried) = inv.0.take_carried() {
            returned.push(carried);
        }
        (pos.0, closed, returned)
    };

    if let Some(open) = closed {
        match open.kind {
            ContainerKind::Chest(chest_pos) => {
                let count = ctx.viewers.close(chest_pos, uuid);
                ctx.out.push(GameOutput::ChestLidUpdated {
                    pos: chest_pos,
                    viewer_count: count,
                });
            }
            ContainerKind::Furnace(furnace_pos) => {
                let _ = ctx.viewers.close(furnace_pos, uuid);
            }
            ContainerKind::CraftingTable { grid, .. } => {
                returned.extend(grid.into_iter().flatten());
            }
        }
    }

    if !returned.is_empty() {
        let mut overflow = Vec::new();
        if let Ok((_, _, _, _, _, _, _, _, _, _, _, mut inv, _, _)) = ctx.players.get_mut(entity) {
            for mut stack in returned {
                let added = inv.0.give_stack(stack);
                if added < stack.count {
                    stack.count -= added;
                    overflow.push(stack);
                }
            }
        }
        for stack in overflow {
            ctx.drops.write(SpawnDropMsg::scatter(
                [pos[0], pos[1] + 0.5, pos[2]],
                stack,
                40,
            ));
        }
        ctx.emit_equipment(uuid, entity);
        ctx.emit_inventory_if_can_interact(uuid, entity);
    }
}

pub(super) fn handle_inventory_click(
    ctx: &mut InteractionCtx,
    uuid: Uuid,
    window: u8,
    state_id: i32,
    click: InventoryClick,
) {
    let Some(entity) = ctx.player_entity(uuid) else {
        return;
    };
    let (can_interact, open) = {
        let Ok((_, _, profile, _, _, _, _, _, _, _, _, _, container, _)) = ctx.players.get(entity)
        else {
            return;
        };
        (profile.can_interact(), container.open.clone())
    };
    if !can_interact {
        return;
    }

    if window != 0 {
        let Some(open) = open else {
            return;
        };
        if open.window_id != window {
            return;
        }
        let next_state_id = open.state_id.wrapping_add(1);
        let world = ctx.world.0.clone();
        match open.kind {
            ContainerKind::Chest(cpos) => {
                let mut dropped: Option<ItemStack> = None;
                let (updated, slots, carried) = {
                    let Ok((_, _, _, _, _, _, _, _, _, _, _, mut inv, mut container, _)) =
                        ctx.players.get_mut(entity)
                    else {
                        return;
                    };
                    let updated = world.update_chest(cpos, |chest| {
                        let (changed, drop) =
                            crate::game::containers::apply_chest_click(chest, &mut inv.0, click);
                        dropped = drop;
                        changed
                    });
                    let Some(chest_items) = updated else {
                        container.open = None;
                        ctx.out.push(GameOutput::ContainerClosed {
                            uuid,
                            window_id: open.window_id,
                        });
                        return;
                    };
                    container.open = Some(OpenContainer {
                        window_id: open.window_id,
                        kind: ContainerKind::Chest(cpos),
                        state_id: next_state_id,
                    });
                    let slots = crate::game::containers::container_slots(&chest_items, &inv.0);
                    (true, slots, inv.0.carried())
                };
                if !updated {
                    return;
                }
                if let Some(stack) = dropped {
                    ctx.throw_from_player(entity, stack);
                }
                refresh_container_viewers(ctx, cpos, uuid);
                ctx.emit_equipment(uuid, entity);
                ctx.out.push(GameOutput::ContainerSynced {
                    uuid,
                    window_id: open.window_id,
                    state_id: next_state_id,
                    slots,
                    carried,
                    furnace_props: None,
                });
                return;
            }
            ContainerKind::CraftingTable {
                mut grid,
                mut output,
            } => {
                let (dropped, slots, carried) = {
                    let Ok((_, _, _, _, _, _, _, _, _, _, _, mut inv, mut container, _)) =
                        ctx.players.get_mut(entity)
                    else {
                        return;
                    };
                    let dropped = crate::game::containers::apply_crafting_table_click(
                        &mut grid,
                        &mut output,
                        &mut inv.0,
                        click,
                    );
                    container.open = Some(OpenContainer {
                        window_id: open.window_id,
                        kind: ContainerKind::CraftingTable { grid, output },
                        state_id: next_state_id,
                    });
                    let slots =
                        crate::game::containers::crafting_table_slots(output, &grid, &inv.0);
                    (dropped, slots, inv.0.carried())
                };
                if let Some(stack) = dropped {
                    ctx.throw_from_player(entity, stack);
                }
                ctx.emit_equipment(uuid, entity);
                ctx.out.push(GameOutput::ContainerSynced {
                    uuid,
                    window_id: open.window_id,
                    state_id: next_state_id,
                    slots,
                    carried,
                    furnace_props: None,
                });
                return;
            }
            ContainerKind::Furnace(fpos) => {
                let mut dropped: Option<ItemStack> = None;
                let (slots, carried, props) = {
                    let Ok((_, _, _, _, _, _, _, _, _, _, _, mut inv, mut container, _)) =
                        ctx.players.get_mut(entity)
                    else {
                        return;
                    };
                    let updated = world.update_furnace(fpos, |furnace| {
                        let (changed, drop) = crate::game::containers::apply_furnace_click(
                            furnace,
                            &mut inv.0,
                            click,
                        );
                        dropped = drop;
                        changed
                    });
                    let Some(furnace) = updated else {
                        container.open = None;
                        ctx.out.push(GameOutput::ContainerClosed {
                            uuid,
                            window_id: open.window_id,
                        });
                        return;
                    };
                    container.open = Some(OpenContainer {
                        window_id: open.window_id,
                        kind: ContainerKind::Furnace(fpos),
                        state_id: next_state_id,
                    });
                    let slots = crate::game::containers::furnace_slots(&furnace, &inv.0);
                    let props = [
                        furnace.burn_time as i16,
                        furnace.burn_total as i16,
                        furnace.cook_time as i16,
                        furnace.cook_total as i16,
                    ];
                    (slots, inv.0.carried(), props)
                };
                if let Some(stack) = dropped {
                    ctx.throw_from_player(entity, stack);
                }
                refresh_container_viewers(ctx, fpos, uuid);
                ctx.emit_equipment(uuid, entity);
                ctx.out.push(GameOutput::ContainerSynced {
                    uuid,
                    window_id: open.window_id,
                    state_id: next_state_id,
                    slots,
                    carried,
                    furnace_props: Some(props),
                });
                return;
            }
        }
    }

    let mut dropped = None;
    let mut changed = false;
    if let Ok((_, _, _, _, _, _, _, _, _, _, _, mut inv, _, _)) = ctx.players.get_mut(entity)
        && window == 0
        && state_id == inv.0.state_id()
    {
        let preview = crate::game::containers::preview_player_drop(&inv.0, click);
        if inv.0.click(click) == crate::game::ClickResult::Changed {
            dropped = preview;
            changed = true;
        }
    }
    if let Some(stack) = dropped {
        ctx.throw_from_player(entity, stack);
    }
    if changed {
        ctx.emit_equipment(uuid, entity);
    }
    ctx.emit_inventory_if_can_interact(uuid, entity);
}
