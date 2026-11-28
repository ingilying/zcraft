//! Block digging, placement, and unified `GameEffects` application.
use super::{
    allocate_window_id,
    containers::refresh_container_viewers,
    emit_equipment, emit_inventory_if_can_interact, throw_from_player,
};
use crate::{
    game::{
        Hand, ItemStack,
        components::{
            Dead, EatingState, Hunger, MiningState, MovementFlags, NetworkEntityId,
            PlayerContainer, PlayerGameMode, PlayerInventory, PlayerViewState, Position, Rotation,
            add_exhaustion, apply_player_dto, player_eye_position, player_in_reach,
            player_intersects_block, snapshot_player_dto,
        },
        io::{ContainerKind, ContainerOpenSync, GameOutput, OpenContainer},
        resources::{ChestViewers, EntityRegistry, OutputQueue, TickClock, VoxelWorld},
        systems::drops::spawn_drop_scatter,
    },
    world::BlockPos,
};
use bevy_ecs::prelude::{Entity, World};
use uuid::Uuid;

#[derive(Clone, Copy)]
enum BlockAction {
    Dig {
        pos: BlockPos,
        face: u8,
        kind: i32,
    },
    Place {
        support: BlockPos,
        face: i32,
        hand: i32,
        hit: [f32; 3],
    },
}

fn interact_block(
    ecs: &mut World,
    uuid: Uuid,
    action: BlockAction,
    sequence: i32,
    tick: u64,
) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let Some(&mode) = ecs.get::<PlayerGameMode>(entity) else {
        return;
    };
    let Some(entity_id) = ecs.get::<NetworkEntityId>(entity).map(|id| id.0) else {
        return;
    };
    let Some(&player_pos) = ecs.get::<Position>(entity) else {
        return;
    };
    let Some(&player_rot) = ecs.get::<Rotation>(entity) else {
        return;
    };
    let Some(&player_flags) = ecs.get::<MovementFlags>(entity) else {
        return;
    };
    let dead = ecs.get::<Dead>(entity).is_some();
    let pending_teleport = ecs
        .get::<PlayerViewState>(entity)
        .is_none_or(|v| v.pending_teleport);
    let can_interact = mode.can_interact();
    let is_survival = mode.is_survival();
    if !can_interact || dead {
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::BlockAckOnly {
                player: uuid,
                sequence,
            });
        return;
    }

    let world = ecs.resource::<VoxelWorld>().0.clone();

    // Survival mining start/cancel without triggering block correction.
    if is_survival && let BlockAction::Dig { pos, face, kind } = action {
        if kind == 1 {
            let cleared = ecs
                .get_entity_mut(entity)
                .ok()
                .and_then(|mut e| e.take::<MiningState>());
            let mut out = ecs.resource_mut::<OutputQueue>();
            if let Some(mining) = cleared {
                out.push(GameOutput::BlockDestruction {
                    entity_id,
                    pos: mining.target,
                    stage: 0xff,
                });
            }
            out.push(GameOutput::BlockAckOnly {
                player: uuid,
                sequence,
            });
            return;
        }
        if kind == 0 {
            let valid = pos.valid()
                && ecs
                    .get::<PlayerViewState>(entity)
                    .is_some_and(|v| v.tracked_chunks.contains(&pos.chunk()));
            let reachable = valid && player_in_reach(player_pos, mode, pos);
            let current = world.block_at(pos);
            if face < 6
                && reachable
                && current != crate::game::AIR_STATE
                && let Some(def) = crate::game::block_by_state(current)
                && def.hardness > 0.0
            {
                if let Ok(mut e) = ecs.get_entity_mut(entity) {
                    e.insert(MiningState {
                        target: pos,
                        start_tick: tick,
                    });
                }
                let mut out = ecs.resource_mut::<OutputQueue>();
                out.push(GameOutput::BlockDestruction {
                    entity_id,
                    pos,
                    stage: 0,
                });
                out.push(GameOutput::BlockAckOnly {
                    player: uuid,
                    sequence,
                });
                return;
            }
        }
    }

    let tracked = ecs
        .get::<PlayerViewState>(entity)
        .map(|v| v.tracked_chunks.clone())
        .unwrap_or_default();
    let valid = |pos: BlockPos| pos.valid() && tracked.contains(&pos.chunk());
    let reachable = |pos: BlockPos| valid(pos) && player_in_reach(player_pos, mode, pos);

    let mut survival_dig_drops: Vec<(BlockPos, ItemStack)> = Vec::new();
    let mut survival_tool_damage: u32 = 0;
    let mut survival_consume_hand: Option<Hand> = None;
    let mut extra_edits: Vec<(BlockPos, i32)> = Vec::new();
    let mut broken_chest_pos: Option<BlockPos> = None;
    let mut broken_chest_items: Vec<ItemStack> = Vec::new();
    let mut clear_break_anim: Option<(i32, BlockPos)> = None;

    let (positions, edit) = match action {
        BlockAction::Dig { pos, face, kind } => {
            let current = world.block_at(pos);
            let edit = if !is_survival {
                if kind == 0 && face < 6 && reachable(pos) && current != crate::game::AIR_STATE {
                    Some((pos, crate::game::AIR_STATE))
                } else {
                    None
                }
            } else if face < 6 && reachable(pos) && current != crate::game::AIR_STATE {
                crate::game::registry().state_by_id(current).and_then(
                    |(block_idx, _block_def, state_def)| {
                        if state_def.hardness < 0.0 {
                            return None;
                        }
                        let tool_stack = ecs
                            .get::<PlayerInventory>(entity)
                            .and_then(|i| i.0.hand(Hand::Main));
                        let tool_item = tool_stack.map(|s| s.item);
                        let can_harvest = !state_def.correct_tool
                            || crate::game::registry().correct_for_drops(tool_item, block_idx);
                        let allowed = if state_def.hardness == 0.0 {
                            kind == 0 || kind == 2
                        } else if kind == 2 {
                            let mut speed = if can_harvest {
                                crate::game::registry()
                                    .tool_speed(tool_item, block_idx)
                                    .max(1.0)
                            } else {
                                1.0
                            };
                            if !player_flags.on_ground {
                                speed /= 5.0;
                            }
                            let eye = player_eye_position(player_pos);
                            let eye_pos = BlockPos([
                                eye[0].floor() as i32,
                                eye[1].floor() as i32,
                                eye[2].floor() as i32,
                            ]);
                            if crate::game::registry()
                                .state_by_id(world.block_at(eye_pos))
                                .is_some_and(|(_, def, _)| def.name == "minecraft:water")
                            {
                                speed /= 5.0;
                            }
                            let expected = f64::from(state_def.hardness)
                                * if can_harvest { 1.5 } else { 5.0 }
                                / f64::from(speed);
                            ecs.get_entity_mut(entity)
                                .ok()
                                .and_then(|mut e| e.take::<MiningState>())
                                .is_some_and(|mining| {
                                    clear_break_anim = Some((entity_id, mining.target));
                                    mining.target == pos
                                        && (tick.saturating_sub(mining.start_tick) as f64) * 0.05
                                            >= expected * 0.7
                                })
                        } else {
                            false
                        };
                        if !allowed {
                            return None;
                        }
                        if state_def.hardness > 0.0 {
                            let is_sword =
                                tool_stack.is_some_and(|s| s.definition().name.ends_with("_sword"));
                            survival_tool_damage = if is_sword { 2 } else { 1 };
                        }
                        if can_harvest {
                            for stack in crate::game::block_drops(current, tool_item) {
                                survival_dig_drops.push((pos, stack));
                            }
                        }
                        Some((pos, crate::game::AIR_STATE))
                    },
                )
            } else {
                None
            };
            if edit.is_some() {
                if is_survival
                    && let Some(mut hunger) = ecs.get_mut::<Hunger>(entity)
                {
                    add_exhaustion(&mut hunger, mode, false, 0.005);
                }
                let drops = crate::game::block_behavior(current).container_drops(&world, pos);
                if !drops.is_empty()
                    || world.chest_items(pos).is_some()
                    || world.furnace_data(pos).is_some()
                {
                    broken_chest_pos = Some(pos);
                    broken_chest_items.extend(drops);
                }
                if let Some(other) = crate::game::paired_block_pos(pos, current)
                    && other.valid()
                    && world.block_at(other) != crate::game::AIR_STATE
                {
                    extra_edits.push((other, crate::game::AIR_STATE));
                }
                for [dx, dy, dz] in [
                    [0, 1, 0],
                    [0, -1, 0],
                    [1, 0, 0],
                    [-1, 0, 0],
                    [0, 0, 1],
                    [0, 0, -1],
                ] {
                    let npos = BlockPos([pos.0[0] + dx, pos.0[1] + dy, pos.0[2] + dz]);
                    if !npos.valid() {
                        continue;
                    }
                    let nstate = world.block_at(npos);
                    if let Some([sx, sy, sz]) = crate::game::required_support_offset(nstate)
                        && BlockPos([npos.0[0] + sx, npos.0[1] + sy, npos.0[2] + sz]) == pos
                    {
                        extra_edits.push((npos, crate::game::AIR_STATE));
                        if is_survival {
                            for stack in crate::game::block_drops(nstate, None) {
                                survival_dig_drops.push((npos, stack));
                            }
                        }
                    }
                }
            }
            ([Some(pos), None], edit)
        }
        BlockAction::Place {
            support,
            face,
            hand,
            hit,
        } => {
            let place = support.on_face(face);
            let edit = place.and_then(|pos| {
                let hand = Hand::from_wire(hand)?;
                let stack = ecs
                    .get::<PlayerInventory>(entity)
                    .and_then(|i| i.0.hand(hand))?;
                let block = stack.definition().block?;
                let support_state = world.block_at(support);
                let target_state = world.block_at(pos);
                let support_block = crate::game::block_by_state(support_state)?;
                let target_block = crate::game::block_by_state(target_state)?;
                if !hit.iter().all(|f| (0.0..=1.0).contains(f))
                    || !reachable(support)
                    || support_block.replaceable
                {
                    return None;
                }
                let placement = crate::game::resolve_placement(
                    block,
                    crate::game::PlacementContext {
                        yaw: player_rot.yaw,
                        pitch: player_rot.pitch,
                        face,
                        hit,
                        support_state,
                        target_state,
                    },
                )?;
                let resolved = match placement {
                    crate::game::PlacementResult::MergeSupport(material) => {
                        let placed = crate::game::block_by_state(material)?;
                        (!placed.collision || !player_intersects_block(player_pos, support))
                            .then_some((support, material))
                    }
                    crate::game::PlacementResult::PlaceTarget(mut material) => {
                        let placed = crate::game::block_by_state(material)?;
                        if let Some([sx, sy, sz]) = crate::game::required_support_offset(material) {
                            let sup_pos = BlockPos([pos.0[0] + sx, pos.0[1] + sy, pos.0[2] + sz]);
                            if !sup_pos.valid()
                                || !crate::game::is_solid_support(world.block_at(sup_pos))
                            {
                                return None;
                            }
                        }
                        material =
                            crate::game::resolve_stairs_shape(pos, material, |p| world.block_at(p));
                        let target_ok = target_block.replaceable
                            || crate::game::registry()
                                .state_by_id(target_state)
                                .is_some_and(|(tb, _, _)| tb == block);
                        (reachable(pos)
                            && target_ok
                            && (!placed.collision || !player_intersects_block(player_pos, pos)))
                        .then_some((pos, material))
                    }
                    crate::game::PlacementResult::PlacePair {
                        primary,
                        secondary_offset: [dx, dy, dz],
                        secondary,
                    } => {
                        let pos2 = BlockPos([pos.0[0] + dx, pos.0[1] + dy, pos.0[2] + dz]);
                        let p1 = crate::game::block_by_state(primary)?;
                        let p2 = crate::game::block_by_state(secondary)?;
                        let t2 = crate::game::block_by_state(world.block_at(pos2))?;
                        let ok = reachable(pos)
                            && valid(pos2)
                            && target_block.replaceable
                            && t2.replaceable
                            && (!p1.collision || !player_intersects_block(player_pos, pos))
                            && (!p2.collision || !player_intersects_block(player_pos, pos2));
                        if ok {
                            extra_edits.push((pos2, secondary));
                            Some((pos, primary))
                        } else {
                            None
                        }
                    }
                };
                if resolved.is_some() && is_survival {
                    survival_consume_hand = Some(hand);
                }
                resolved
            });
            ([Some(support), place], edit)
        }
    };

    if let Some((eid, mpos)) = clear_break_anim {
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::BlockDestruction {
                entity_id: eid,
                pos: mpos,
                stage: 0xff,
            });
    }

    if !pending_teleport
        && let Some((pos, material)) = edit
        && let Some(mutation) = world.set_block(pos, material)
    {
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::WorldMutatedWithAck {
                actor: uuid,
                chunk: pos.chunk(),
                mutation,
                sequence,
            });

        for (epos, estate) in extra_edits {
            if let Some(m) = world.set_block(epos, estate) {
                ecs.resource_mut::<OutputQueue>()
                    .push(GameOutput::WorldMutated(m));
            }
        }
        for [dx, dz] in [[1, 0], [-1, 0], [0, 1], [0, -1]] {
            let npos = BlockPos([pos.0[0] + dx, pos.0[1], pos.0[2] + dz]);
            if npos.valid() {
                let nstate = world.block_at(npos);
                let updated =
                    crate::game::resolve_stairs_shape(npos, nstate, |p| world.block_at(p));
                if updated != nstate
                    && let Some(m) = world.set_block(npos, updated)
                {
                    ecs.resource_mut::<OutputQueue>()
                        .push(GameOutput::WorldMutated(m));
                }
            }
        }

        if let Some(cpos) = broken_chest_pos {
            let drop_xyz = [
                f64::from(cpos.0[0]) + 0.5,
                f64::from(cpos.0[1]) + 0.5,
                f64::from(cpos.0[2]) + 0.5,
            ];
            for stack in broken_chest_items {
                spawn_drop_scatter(ecs, drop_xyz, stack, 10);
            }
            refresh_container_viewers(ecs, cpos, Uuid::nil());
        }

        let mut inv_changed = false;
        if survival_tool_damage > 0
            && let Some(mut inv) = ecs.get_mut::<PlayerInventory>(entity)
        {
            inv_changed |= inv.0.damage_hand(Hand::Main, survival_tool_damage)
                == crate::game::ClickResult::Changed;
        }
        if let Some(hand) = survival_consume_hand
            && let Some(mut inv) = ecs.get_mut::<PlayerInventory>(entity)
        {
            inv_changed |= inv.0.drop_hand(hand, false) == crate::game::ClickResult::Changed;
        }
        for (drop_pos, stack) in survival_dig_drops {
            let drop_xyz = [
                f64::from(drop_pos.0[0]) + 0.5,
                f64::from(drop_pos.0[1]) + 0.5,
                f64::from(drop_pos.0[2]) + 0.5,
            ];
            spawn_drop_scatter(ecs, drop_xyz, stack, 10);
        }
        if inv_changed {
            emit_equipment(ecs, uuid, entity);
            emit_inventory_if_can_interact(ecs, uuid, entity);
        }
        return;
    }

    let corrections: Vec<(BlockPos, i32)> = positions
        .into_iter()
        .flatten()
        .filter(|&pos| valid(pos))
        .map(|pos| (pos, world.block_at(pos)))
        .collect();
    ecs.resource_mut::<OutputQueue>()
        .push(GameOutput::BlockCorrected {
            player: uuid,
            corrections,
            sequence,
        });
}

pub(crate) fn handle_dig(
    ecs: &mut World,
    uuid: Uuid,
    action: i32,
    pos: BlockPos,
    face: u8,
    sequence: i32,
    tick: u64,
) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    match action {
        0..=2 => interact_block(
            ecs,
            uuid,
            BlockAction::Dig {
                pos,
                face,
                kind: action,
            },
            sequence,
            tick,
        ),
        3 | 4 => {
            let all = action == 3;
            let dropped = {
                let Some(mut inv) = ecs.get_mut::<PlayerInventory>(entity) else {
                    return;
                };
                let preview = inv.0.hand(Hand::Main).map(|mut s| {
                    if !all {
                        s.count = 1;
                    }
                    s
                });
                if inv.0.drop_hand(Hand::Main, all) == crate::game::ClickResult::Changed {
                    preview
                } else {
                    None
                }
            };
            if let Some(stack) = dropped {
                throw_from_player(ecs, entity, stack);
            }
            emit_equipment(ecs, uuid, entity);
            emit_inventory_if_can_interact(ecs, uuid, entity);
        }
        5 => {
            if let Ok(mut e) = ecs.get_entity_mut(entity) {
                e.remove::<EatingState>();
            }
        }
        6 => {
            if let Some(mut inv) = ecs.get_mut::<PlayerInventory>(entity) {
                inv.0.swap_hands();
            }
            emit_equipment(ecs, uuid, entity);
            emit_inventory_if_can_interact(ecs, uuid, entity);
        }
        _ => emit_inventory_if_can_interact(ecs, uuid, entity),
    }
}

pub(crate) fn handle_place(
    ecs: &mut World,
    uuid: Uuid,
    pos: BlockPos,
    face: i32,
    hand: i32,
    hit: [f32; 3],
    sequence: i32,
) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let world = ecs.resource::<VoxelWorld>().0.clone();
    let can_use_block = ecs
        .get::<PlayerGameMode>(entity)
        .zip(ecs.get::<Position>(entity))
        .zip(ecs.get::<MovementFlags>(entity))
        .is_some_and(|((&mode, &ppos), &flags)| {
            mode.can_interact()
                && ecs.get::<Dead>(entity).is_none()
                && !flags.sneaking
                && player_in_reach(ppos, mode, pos)
        })
        && ecs
            .get::<PlayerViewState>(entity)
            .is_some_and(|v| !v.pending_teleport);

    let tick = ecs.resource::<TickClock>().tick;
    let effects = if can_use_block
        && let Some(mut player) = snapshot_player_dto(ecs, entity)
    {
        let clicked_state = world.block_at(pos);
        let mut inv = ecs.get::<PlayerInventory>(entity).unwrap().0.clone();
        let mut use_ctx = crate::game::UseContext {
            world: &world,
            player: &mut player,
            inventory: &mut inv,
            tick,
        };
        let fx = crate::game::block_behavior(clicked_state).on_use(&mut use_ctx, pos, clicked_state);
        apply_player_dto(ecs, entity, &player);
        ecs.get_mut::<PlayerInventory>(entity).unwrap().0 = inv;
        fx
    } else {
        crate::game::GameEffects::pass()
    };

    if effects.handled {
        apply_game_effects(ecs, uuid, entity, effects, sequence);
        return;
    }

    interact_block(
        ecs,
        uuid,
        BlockAction::Place {
            support: pos,
            face,
            hand,
            hit,
        },
        sequence,
        tick,
    );
}

pub(super) fn apply_game_effects(
    ecs: &mut World,
    uuid: Uuid,
    entity: Entity,
    effects: crate::game::GameEffects,
    sequence: i32,
) {
    let world = ecs.resource::<VoxelWorld>().0.clone();
    for (upos, ustate) in effects.block_updates {
        if let Some(m) = world.set_block(upos, ustate) {
            ecs.resource_mut::<OutputQueue>()
                .push(GameOutput::WorldMutated(m));
        }
    }
    if let Some(day_time) = effects.set_day_time {
        let time = world.set_day_time(day_time);
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::TimeUpdated(time));
    }
    if let Some(duration) = effects.clear_weather_ticks {
        for event in world.set_weather(false, false, duration) {
            ecs.resource_mut::<OutputQueue>()
                .push(GameOutput::WeatherChanged(event));
        }
    }
    for msg in effects.broadcast_messages {
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::BroadcastSystemChat(msg));
    }
    if !effects.player_drops.is_empty()
        && let Some(ppos) = ecs.get::<Position>(entity).map(|p| p.0)
    {
        for stack in effects.player_drops {
            spawn_drop_scatter(ecs, [ppos[0], ppos[1] + 0.5, ppos[2]], stack, 10);
        }
    }
    for message in effects.player_messages {
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerSystemMessage { uuid, message });
    }

    if let Some(screen) = effects.open_screen {
        let inv = ecs.get::<PlayerInventory>(entity).unwrap().0.clone();
        match screen {
            crate::game::ContainerScreen::CraftingTable => {
                let window_id = allocate_window_id(ecs, entity);
                ecs.get_mut::<PlayerContainer>(entity).unwrap().open = Some(OpenContainer {
                    window_id,
                    kind: ContainerKind::CraftingTable {
                        grid: [None; 9],
                        output: None,
                    },
                    state_id: 0,
                });
                let slots =
                    crate::game::containers::crafting_table_slots(None, &[None; 9], &inv);
                ecs.resource_mut::<OutputQueue>()
                    .push(GameOutput::ContainerOpened {
                        uuid,
                        window_id,
                        screen: ContainerOpenSync::CraftingTable {
                            slots,
                            carried: inv.carried(),
                        },
                    });
            }
            crate::game::ContainerScreen::Furnace(fpos) => {
                if let Some(furnace) = world.furnace_data(fpos) {
                    let window_id = allocate_window_id(ecs, entity);
                    ecs.get_mut::<PlayerContainer>(entity).unwrap().open = Some(OpenContainer {
                        window_id,
                        kind: ContainerKind::Furnace(fpos),
                        state_id: 0,
                    });
                    let _ = ecs.resource_mut::<ChestViewers>().open(fpos, uuid);
                    let slots = crate::game::containers::furnace_slots(&furnace, &inv);
                    let properties = [
                        furnace.burn_time as i16,
                        furnace.burn_total as i16,
                        furnace.cook_time as i16,
                        furnace.cook_total as i16,
                    ];
                    ecs.resource_mut::<OutputQueue>()
                        .push(GameOutput::ContainerOpened {
                            uuid,
                            window_id,
                            screen: ContainerOpenSync::Furnace {
                                slots,
                                carried: inv.carried(),
                                properties,
                            },
                        });
                }
            }
            crate::game::ContainerScreen::Chest(cpos) => {
                if let Some(chest_items) = world.chest_items(cpos) {
                    let window_id = allocate_window_id(ecs, entity);
                    ecs.get_mut::<PlayerContainer>(entity).unwrap().open = Some(OpenContainer {
                        window_id,
                        kind: ContainerKind::Chest(cpos),
                        state_id: 0,
                    });
                    let viewer_count = ecs.resource_mut::<ChestViewers>().open(cpos, uuid);
                    let slots = crate::game::containers::container_slots(&chest_items, &inv);
                    let mut out = ecs.resource_mut::<OutputQueue>();
                    out.push(GameOutput::ChestLidUpdated {
                        pos: cpos,
                        viewer_count,
                    });
                    out.push(GameOutput::ContainerOpened {
                        uuid,
                        window_id,
                        screen: ContainerOpenSync::Chest {
                            slots,
                            carried: inv.carried(),
                        },
                    });
                }
            }
        }
    }

    if effects.inventory_changed {
        emit_equipment(ecs, uuid, entity);
        let inv = ecs.get::<PlayerInventory>(entity).unwrap().0.clone();
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerInventorySynced {
                uuid,
                inventory: inv,
            });
    }

    ecs.resource_mut::<OutputQueue>()
        .push(GameOutput::BlockAckOnly {
            player: uuid,
            sequence,
        });
}
