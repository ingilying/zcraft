//! Block digging, placement, and unified `GameEffects` application.
use super::{InteractionCtx, containers::refresh_container_viewers};
use crate::{
    game::{
        Hand, ItemStack,
        components::{
            EatingState, MiningState, add_exhaustion, player_eye_position, player_in_reach,
            player_intersects_block,
        },
        events::SpawnDropMsg,
        io::{ContainerKind, ContainerOpenSync, GameOutput, OpenContainer},
    },
    world::BlockPos,
};
use bevy_ecs::prelude::Entity;
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
    ctx: &mut InteractionCtx,
    uuid: Uuid,
    action: BlockAction,
    sequence: i32,
    tick: u64,
) {
    let Some(entity) = ctx.player_entity(uuid) else {
        return;
    };
    let (
        entity_id,
        profile,
        player_pos,
        player_rot,
        player_move,
        dead,
        mining_state,
        pending_teleport,
        tracked,
        main_hand_stack,
        place_hand_stack,
    ) = {
        let Ok((
            net_id,
            _,
            profile,
            &pos,
            &rot,
            &movement,
            _,
            _,
            dead,
            _,
            mining,
            inv,
            _,
            view,
        )) = ctx.players.get(entity)
        else {
            return;
        };
        let place_stack = match action {
            BlockAction::Place { hand, .. } => Hand::from_wire(hand).and_then(|h| inv.0.hand(h)),
            BlockAction::Dig { .. } => None,
        };
        (
            net_id.0,
            profile.clone(),
            pos,
            rot,
            movement,
            dead.is_some(),
            mining.copied(),
            view.pending_teleport,
            view.tracked_chunks.clone(),
            inv.0.hand(Hand::Main),
            place_stack,
        )
    };

    let can_interact = profile.can_interact();
    let is_survival = profile.is_survival();
    if !can_interact || dead {
        ctx.out.push(GameOutput::BlockAckOnly {
            player: uuid,
            sequence,
        });
        return;
    }

    let world = ctx.world.0.clone();

    // Survival mining start/cancel without triggering block correction.
    if is_survival && let BlockAction::Dig { pos, face, kind } = action {
        if kind == 1 {
            if let Some(mining) = mining_state {
                ctx.commands.entity(entity).remove::<MiningState>();
                ctx.out.push(GameOutput::BlockDestruction {
                    entity_id,
                    pos: mining.target,
                    stage: 0xff,
                });
            }
            ctx.out.push(GameOutput::BlockAckOnly {
                player: uuid,
                sequence,
            });
            return;
        }
        if kind == 0 {
            let valid = pos.valid() && tracked.contains(&pos.chunk());
            let reachable = valid && player_in_reach(player_pos, &profile, pos);
            let current = world.block_at(pos);
            if face < 6
                && reachable
                && current != crate::game::AIR_STATE
                && let Some(def) = crate::game::block_by_state(current)
                && def.hardness > 0.0
            {
                ctx.commands.entity(entity).insert(MiningState {
                    target: pos,
                    start_tick: tick,
                });
                ctx.out.push(GameOutput::BlockDestruction {
                    entity_id,
                    pos,
                    stage: 0,
                });
                ctx.out.push(GameOutput::BlockAckOnly {
                    player: uuid,
                    sequence,
                });
                return;
            }
        }
    }

    let valid = |pos: BlockPos| pos.valid() && tracked.contains(&pos.chunk());
    let reachable = |pos: BlockPos| valid(pos) && player_in_reach(player_pos, &profile, pos);

    let mut survival_dig_drops: Vec<(BlockPos, ItemStack)> = Vec::new();
    let mut survival_tool_damage: u32 = 0;
    let mut survival_consume_hand: Option<Hand> = None;
    let mut extra_edits: Vec<(BlockPos, i32)> = Vec::new();
    let mut broken_chest_pos: Option<BlockPos> = None;
    let mut broken_chest_items: Vec<ItemStack> = Vec::new();
    let mut clear_break_anim: Option<(i32, BlockPos)> = None;
    let mut clear_mining_comp = false;

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
                        let tool_stack = main_hand_stack;
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
                            if !player_move.on_ground {
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
                            mining_state.is_some_and(|mining| {
                                clear_mining_comp = true;
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
                    && let Ok((_, _, _, _, _, _, _, mut hunger, ..)) = ctx.players.get_mut(entity)
                {
                    add_exhaustion(&mut hunger, true, false, 0.005);
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
                let stack = place_hand_stack?;
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

    if clear_mining_comp {
        ctx.commands.entity(entity).remove::<MiningState>();
    }
    if let Some((eid, mpos)) = clear_break_anim {
        ctx.out.push(GameOutput::BlockDestruction {
            entity_id: eid,
            pos: mpos,
            stage: 0xff,
        });
    }

    if !pending_teleport
        && let Some((pos, material)) = edit
        && let Some(mutation) = world.set_block(pos, material)
    {
        ctx.out.push(GameOutput::WorldMutatedWithAck {
            actor: uuid,
            chunk: pos.chunk(),
            mutation,
            sequence,
        });

        for (epos, estate) in extra_edits {
            if let Some(m) = world.set_block(epos, estate) {
                ctx.out.push(GameOutput::WorldMutated(m));
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
                    ctx.out.push(GameOutput::WorldMutated(m));
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
                ctx.drops
                    .write(SpawnDropMsg::scatter(drop_xyz, stack, 10));
            }
            refresh_container_viewers(ctx, cpos, Uuid::nil());
        }

        let mut inv_changed = false;
        if let Ok((_, _, _, _, _, _, _, _, _, _, _, mut inv, _, _)) = ctx.players.get_mut(entity) {
            if survival_tool_damage > 0 {
                inv_changed |= inv.0.damage_hand(Hand::Main, survival_tool_damage)
                    == crate::game::ClickResult::Changed;
            }
            if let Some(hand) = survival_consume_hand {
                inv_changed |= inv.0.drop_hand(hand, false) == crate::game::ClickResult::Changed;
            }
        }
        for (drop_pos, stack) in survival_dig_drops {
            let drop_xyz = [
                f64::from(drop_pos.0[0]) + 0.5,
                f64::from(drop_pos.0[1]) + 0.5,
                f64::from(drop_pos.0[2]) + 0.5,
            ];
            ctx.drops
                .write(SpawnDropMsg::scatter(drop_xyz, stack, 10));
        }
        if inv_changed {
            ctx.emit_equipment(uuid, entity);
            ctx.emit_inventory_if_can_interact(uuid, entity);
        }
        return;
    }

    let corrections: Vec<(BlockPos, i32)> = positions
        .into_iter()
        .flatten()
        .filter(|&pos| valid(pos))
        .map(|pos| (pos, world.block_at(pos)))
        .collect();
    ctx.out.push(GameOutput::BlockCorrected {
        player: uuid,
        corrections,
        sequence,
    });
}

pub(super) fn handle_dig(
    ctx: &mut InteractionCtx,
    uuid: Uuid,
    action: i32,
    pos: BlockPos,
    face: u8,
    sequence: i32,
    tick: u64,
) {
    let Some(entity) = ctx.player_entity(uuid) else {
        return;
    };
    match action {
        0..=2 => interact_block(
            ctx,
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
                let Ok((_, _, _, _, _, _, _, _, _, _, _, mut inv, _, _)) =
                    ctx.players.get_mut(entity)
                else {
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
                ctx.throw_from_player(entity, stack);
            }
            ctx.emit_equipment(uuid, entity);
            ctx.emit_inventory_if_can_interact(uuid, entity);
        }
        5 => {
            ctx.commands.entity(entity).remove::<EatingState>();
        }
        6 => {
            if let Ok((_, _, _, _, _, _, _, _, _, _, _, mut inv, _, _)) = ctx.players.get_mut(entity)
            {
                inv.0.swap_hands();
            }
            ctx.emit_equipment(uuid, entity);
            ctx.emit_inventory_if_can_interact(uuid, entity);
        }
        _ => ctx.emit_inventory_if_can_interact(uuid, entity),
    }
}

pub(super) fn handle_place(
    ctx: &mut InteractionCtx,
    uuid: Uuid,
    pos: BlockPos,
    face: i32,
    hand: i32,
    hit: [f32; 3],
    sequence: i32,
) {
    let Some(entity) = ctx.player_entity(uuid) else {
        return;
    };
    let can_use_block = ctx.players.get(entity).is_ok_and(
        |(_, _, profile, &ppos, _, &movement, _, _, dead, _, _, _, _, view)| {
            profile.can_interact()
                && dead.is_none()
                && !movement.sneaking
                && player_in_reach(ppos, profile, pos)
                && !view.pending_teleport
        },
    );

    let tick = ctx.clock.tick;
    let effects = if can_use_block {
        let clicked_state = ctx.world.0.block_at(pos);
        ctx.with_use_context(entity, |use_ctx| {
            crate::game::block_behavior(clicked_state).on_use(use_ctx, pos, clicked_state)
        })
        .unwrap_or_else(crate::game::GameEffects::pass)
    } else {
        crate::game::GameEffects::pass()
    };

    if effects.handled {
        apply_game_effects(ctx, uuid, entity, effects, sequence);
        return;
    }

    interact_block(
        ctx,
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
    ctx: &mut InteractionCtx,
    uuid: Uuid,
    entity: Entity,
    effects: crate::game::GameEffects,
    sequence: i32,
) {
    let world = ctx.world.0.clone();
    for (upos, ustate) in effects.block_updates {
        if let Some(m) = world.set_block(upos, ustate) {
            ctx.out.push(GameOutput::WorldMutated(m));
        }
    }
    if let Some(day_time) = effects.set_day_time {
        let time = world.set_day_time(day_time);
        ctx.out.push(GameOutput::TimeUpdated(time));
    }
    if let Some(duration) = effects.clear_weather_ticks {
        for event in world.set_weather(false, false, duration) {
            ctx.out.push(GameOutput::WeatherChanged(event));
        }
    }
    for msg in effects.broadcast_messages {
        ctx.out.push(GameOutput::BroadcastSystemChat(msg));
    }
    if !effects.player_drops.is_empty()
        && let Ok((_, _, _, &ppos, ..)) = ctx.players.get(entity)
    {
        for stack in effects.player_drops {
            ctx.drops.write(SpawnDropMsg::scatter(
                [ppos.0[0], ppos.0[1] + 0.5, ppos.0[2]],
                stack,
                10,
            ));
        }
    }
    for message in effects.player_messages {
        ctx.out.push(GameOutput::PlayerSystemMessage { uuid, message });
    }

    if let Some(screen) = effects.open_screen {
        let inv = ctx
            .players
            .get(entity)
            .map(|(_, _, _, _, _, _, _, _, _, _, _, inv, _, _)| inv.0.clone())
            .expect("player inventory");
        match screen {
            crate::game::ContainerScreen::CraftingTable => {
                let window_id = ctx.allocate_window_id(entity);
                if let Ok((_, _, _, _, _, _, _, _, _, _, _, _, mut container, _)) =
                    ctx.players.get_mut(entity)
                {
                    container.open = Some(OpenContainer {
                        window_id,
                        kind: ContainerKind::CraftingTable {
                            grid: [None; 9],
                            output: None,
                        },
                        state_id: 0,
                    });
                }
                let slots = crate::game::containers::crafting_table_slots(None, &[None; 9], &inv);
                ctx.out.push(GameOutput::ContainerOpened {
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
                    let window_id = ctx.allocate_window_id(entity);
                    if let Ok((_, _, _, _, _, _, _, _, _, _, _, _, mut container, _)) =
                        ctx.players.get_mut(entity)
                    {
                        container.open = Some(OpenContainer {
                            window_id,
                            kind: ContainerKind::Furnace(fpos),
                            state_id: 0,
                        });
                    }
                    let _ = ctx.viewers.open(fpos, uuid);
                    let slots = crate::game::containers::furnace_slots(&furnace, &inv);
                    let properties = [
                        furnace.burn_time as i16,
                        furnace.burn_total as i16,
                        furnace.cook_time as i16,
                        furnace.cook_total as i16,
                    ];
                    ctx.out.push(GameOutput::ContainerOpened {
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
                    let window_id = ctx.allocate_window_id(entity);
                    if let Ok((_, _, _, _, _, _, _, _, _, _, _, _, mut container, _)) =
                        ctx.players.get_mut(entity)
                    {
                        container.open = Some(OpenContainer {
                            window_id,
                            kind: ContainerKind::Chest(cpos),
                            state_id: 0,
                        });
                    }
                    let viewer_count = ctx.viewers.open(cpos, uuid);
                    let slots = crate::game::containers::container_slots(&chest_items, &inv);
                    ctx.out.push(GameOutput::ChestLidUpdated {
                        pos: cpos,
                        viewer_count,
                    });
                    ctx.out.push(GameOutput::ContainerOpened {
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
        ctx.emit_equipment(uuid, entity);
        if let Ok((_, _, _, _, _, _, _, _, _, _, _, inv, _, _)) = ctx.players.get(entity) {
            ctx.out.push(GameOutput::PlayerInventorySynced {
                uuid,
                inventory: inv.0.clone(),
            });
        }
    }

    ctx.out.push(GameOutput::BlockAckOnly {
        player: uuid,
        sequence,
    });
}
