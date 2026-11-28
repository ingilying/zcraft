//! Container window lifecycle, multi-viewer sync, and inventory/container clicks.
use super::{emit_equipment, emit_inventory_if_can_interact, throw_from_player};
use crate::{
    game::{
        InventoryClick, ItemStack,
        components::{PlayerContainer, PlayerGameMode, PlayerInventory, Position},
        io::{ContainerKind, GameOutput, OpenContainer},
        resources::{ChestViewers, EntityRegistry, OutputQueue, VoxelWorld},
        systems::drops::spawn_drop_scatter,
    },
    world::BlockPos,
};
use bevy_ecs::prelude::World;
use uuid::Uuid;

pub(crate) fn refresh_container_viewers(ecs: &mut World, pos: BlockPos, exclude: Uuid) {
    let others = ecs.resource::<ChestViewers>().others(pos, exclude);
    if others.is_empty() {
        return;
    }
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let chest_items = world.chest_items(pos);
    let furnace_data = world.furnace_data(pos);

    for viewer_uuid in others {
        let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&viewer_uuid) else {
            continue;
        };
        let Some(open) = ecs
            .get::<PlayerContainer>(entity)
            .and_then(|c| c.open.clone())
        else {
            continue;
        };
        let Some(inv) = ecs.get::<PlayerInventory>(entity).map(|i| i.0.clone()) else {
            continue;
        };
        match open.kind {
            ContainerKind::Chest(cpos) if cpos == pos => {
                if let Some(items) = &chest_items {
                    let next_state_id = open.state_id.wrapping_add(1);
                    if let Some(mut c) = ecs.get_mut::<PlayerContainer>(entity) {
                        c.open = Some(OpenContainer {
                            state_id: next_state_id,
                            ..open
                        });
                    }
                    let slots = crate::game::containers::container_slots(items, &inv);
                    ecs.resource_mut::<OutputQueue>()
                        .push(GameOutput::ContainerSynced {
                            uuid: viewer_uuid,
                            window_id: open.window_id,
                            state_id: next_state_id,
                            slots,
                            carried: inv.carried(),
                            furnace_props: None,
                        });
                } else {
                    if let Some(mut c) = ecs.get_mut::<PlayerContainer>(entity) {
                        c.open = None;
                    }
                    ecs.resource_mut::<OutputQueue>()
                        .push(GameOutput::ContainerClosed {
                            uuid: viewer_uuid,
                            window_id: open.window_id,
                        });
                }
            }
            ContainerKind::Furnace(fpos) if fpos == pos => {
                if let Some(furnace) = &furnace_data {
                    let next_state_id = open.state_id.wrapping_add(1);
                    if let Some(mut c) = ecs.get_mut::<PlayerContainer>(entity) {
                        c.open = Some(OpenContainer {
                            state_id: next_state_id,
                            ..open
                        });
                    }
                    let slots = crate::game::containers::furnace_slots(furnace, &inv);
                    let props = [
                        furnace.burn_time as i16,
                        furnace.burn_total as i16,
                        furnace.cook_time as i16,
                        furnace.cook_total as i16,
                    ];
                    ecs.resource_mut::<OutputQueue>()
                        .push(GameOutput::ContainerSynced {
                            uuid: viewer_uuid,
                            window_id: open.window_id,
                            state_id: next_state_id,
                            slots,
                            carried: inv.carried(),
                            furnace_props: Some(props),
                        });
                } else {
                    if let Some(mut c) = ecs.get_mut::<PlayerContainer>(entity) {
                        c.open = None;
                    }
                    ecs.resource_mut::<OutputQueue>()
                        .push(GameOutput::ContainerClosed {
                            uuid: viewer_uuid,
                            window_id: open.window_id,
                        });
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn handle_close_window(ecs: &mut World, uuid: Uuid, window_id: u8) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let pos = ecs.get::<Position>(entity).unwrap().0;
    let closed = {
        let mut c = ecs.get_mut::<PlayerContainer>(entity).unwrap();
        if let Some(open) = &c.open
            && (window_id == 0 || open.window_id == window_id)
        {
            c.open.take()
        } else {
            None
        }
    };
    let mut returned = Vec::new();
    if let Some(open) = closed {
        match open.kind {
            ContainerKind::Chest(chest_pos) => {
                let count = ecs.resource_mut::<ChestViewers>().close(chest_pos, uuid);
                ecs.resource_mut::<OutputQueue>()
                    .push(GameOutput::ChestLidUpdated {
                        pos: chest_pos,
                        viewer_count: count,
                    });
            }
            ContainerKind::Furnace(furnace_pos) => {
                let _ = ecs.resource_mut::<ChestViewers>().close(furnace_pos, uuid);
            }
            ContainerKind::CraftingTable { grid, .. } => {
                returned.extend(grid.into_iter().flatten());
            }
        }
    }
    {
        let mut inv = ecs.get_mut::<PlayerInventory>(entity).unwrap();
        if window_id == 0 {
            returned.extend(inv.0.drain_crafting_grid());
        }
        if let Some(carried) = inv.0.take_carried() {
            returned.push(carried);
        }
    }
    if !returned.is_empty() {
        let mut overflow = Vec::new();
        {
            let mut inv = ecs.get_mut::<PlayerInventory>(entity).unwrap();
            for mut stack in returned {
                let added = inv.0.give_stack(stack);
                if added < stack.count {
                    stack.count -= added;
                    overflow.push(stack);
                }
            }
        }
        for stack in overflow {
            spawn_drop_scatter(ecs, [pos[0], pos[1] + 0.5, pos[2]], stack, 40);
        }
        emit_equipment(ecs, uuid, entity);
        emit_inventory_if_can_interact(ecs, uuid, entity);
    }
}

pub(crate) fn handle_inventory_click(
    ecs: &mut World,
    uuid: Uuid,
    window: u8,
    state_id: i32,
    click: InventoryClick,
) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if !ecs
        .get::<PlayerGameMode>(entity)
        .is_some_and(|gm| gm.can_interact())
    {
        return;
    }

    if window != 0 {
        let Some(open) = ecs
            .get::<PlayerContainer>(entity)
            .and_then(|c| c.open.clone())
        else {
            return;
        };
        if open.window_id != window {
            return;
        }
        let next_state_id = open.state_id.wrapping_add(1);
        let world = ecs.resource::<VoxelWorld>().0.clone();
        match open.kind {
            ContainerKind::Chest(cpos) => {
                let mut inv = ecs.get::<PlayerInventory>(entity).unwrap().0.clone();
                let mut dropped: Option<ItemStack> = None;
                let updated = world.update_chest(cpos, |chest| {
                    let (changed, drop) =
                        crate::game::containers::apply_chest_click(chest, &mut inv, click);
                    dropped = drop;
                    changed
                });
                ecs.get_mut::<PlayerInventory>(entity).unwrap().0 = inv.clone();
                let Some(chest_items) = updated else {
                    ecs.get_mut::<PlayerContainer>(entity).unwrap().open = None;
                    ecs.resource_mut::<OutputQueue>()
                        .push(GameOutput::ContainerClosed {
                            uuid,
                            window_id: open.window_id,
                        });
                    return;
                };
                if let Some(stack) = dropped {
                    throw_from_player(ecs, entity, stack);
                }
                refresh_container_viewers(ecs, cpos, uuid);
                ecs.get_mut::<PlayerContainer>(entity).unwrap().open = Some(OpenContainer {
                    window_id: open.window_id,
                    kind: ContainerKind::Chest(cpos),
                    state_id: next_state_id,
                });
                emit_equipment(ecs, uuid, entity);
                let slots = crate::game::containers::container_slots(&chest_items, &inv);
                ecs.resource_mut::<OutputQueue>()
                    .push(GameOutput::ContainerSynced {
                        uuid,
                        window_id: open.window_id,
                        state_id: next_state_id,
                        slots,
                        carried: inv.carried(),
                        furnace_props: None,
                    });
                return;
            }
            ContainerKind::CraftingTable {
                mut grid,
                mut output,
            } => {
                let mut inv = ecs.get::<PlayerInventory>(entity).unwrap().0.clone();
                let dropped = crate::game::containers::apply_crafting_table_click(
                    &mut grid,
                    &mut output,
                    &mut inv,
                    click,
                );
                ecs.get_mut::<PlayerInventory>(entity).unwrap().0 = inv.clone();
                if let Some(stack) = dropped {
                    throw_from_player(ecs, entity, stack);
                }
                ecs.get_mut::<PlayerContainer>(entity).unwrap().open = Some(OpenContainer {
                    window_id: open.window_id,
                    kind: ContainerKind::CraftingTable { grid, output },
                    state_id: next_state_id,
                });
                emit_equipment(ecs, uuid, entity);
                let slots = crate::game::containers::crafting_table_slots(output, &grid, &inv);
                ecs.resource_mut::<OutputQueue>()
                    .push(GameOutput::ContainerSynced {
                        uuid,
                        window_id: open.window_id,
                        state_id: next_state_id,
                        slots,
                        carried: inv.carried(),
                        furnace_props: None,
                    });
                return;
            }
            ContainerKind::Furnace(fpos) => {
                let mut inv = ecs.get::<PlayerInventory>(entity).unwrap().0.clone();
                let mut dropped: Option<ItemStack> = None;
                let updated = world.update_furnace(fpos, |furnace| {
                    let (changed, drop) =
                        crate::game::containers::apply_furnace_click(furnace, &mut inv, click);
                    dropped = drop;
                    changed
                });
                ecs.get_mut::<PlayerInventory>(entity).unwrap().0 = inv.clone();
                let Some(furnace) = updated else {
                    ecs.get_mut::<PlayerContainer>(entity).unwrap().open = None;
                    ecs.resource_mut::<OutputQueue>()
                        .push(GameOutput::ContainerClosed {
                            uuid,
                            window_id: open.window_id,
                        });
                    return;
                };
                if let Some(stack) = dropped {
                    throw_from_player(ecs, entity, stack);
                }
                refresh_container_viewers(ecs, fpos, uuid);
                ecs.get_mut::<PlayerContainer>(entity).unwrap().open = Some(OpenContainer {
                    window_id: open.window_id,
                    kind: ContainerKind::Furnace(fpos),
                    state_id: next_state_id,
                });
                emit_equipment(ecs, uuid, entity);
                let slots = crate::game::containers::furnace_slots(&furnace, &inv);
                let props = [
                    furnace.burn_time as i16,
                    furnace.burn_total as i16,
                    furnace.cook_time as i16,
                    furnace.cook_total as i16,
                ];
                ecs.resource_mut::<OutputQueue>()
                    .push(GameOutput::ContainerSynced {
                        uuid,
                        window_id: open.window_id,
                        state_id: next_state_id,
                        slots,
                        carried: inv.carried(),
                        furnace_props: Some(props),
                    });
                return;
            }
        }
    }

    let mut dropped = None;
    let mut changed = false;
    {
        let mut inv = ecs.get_mut::<PlayerInventory>(entity).unwrap();
        if window == 0 && state_id == inv.0.state_id() {
            let preview = crate::game::containers::preview_player_drop(&inv.0, click);
            if inv.0.click(click) == crate::game::ClickResult::Changed {
                dropped = preview;
                changed = true;
            }
        }
    }
    if let Some(stack) = dropped {
        throw_from_player(ecs, entity, stack);
    }
    if changed {
        emit_equipment(ecs, uuid, entity);
    }
    emit_inventory_if_can_interact(ecs, uuid, entity);
}
