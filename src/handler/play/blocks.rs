//! Server-authoritative inventory, item use, placement, and mining.
#[cfg(test)]
#[path = "blocks_tests.rs"]
mod tests;
use super::{Context, Outcome, play};
use crate::{
    game::Hand,
    players::Reservation,
    protocol::{
        Clientbound,
        packets::{play::HeldItemSlot, play_blocks::*},
    },
    world::{BlockPos, Chunk, World},
};
use std::{collections::HashMap, io, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ContainerKind {
    Chest(BlockPos),
    CraftingTable {
        grid: [Option<crate::game::ItemStack>; 9],
        output: Option<crate::game::ItemStack>,
    },
    Furnace(BlockPos),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OpenContainer {
    pub window_id: u8,
    pub kind: ContainerKind,
    pub state_id: i32,
}

pub(super) fn furnace_property_packets(
    window_id: u8,
    furnace: &crate::world::FurnaceData,
) -> [Clientbound; 4] {
    [
        ContainerSetData {
            window_id,
            property: 0,
            value: furnace.burn_time as i16,
        }
        .into(),
        ContainerSetData {
            window_id,
            property: 1,
            value: furnace.burn_total as i16,
        }
        .into(),
        ContainerSetData {
            window_id,
            property: 2,
            value: furnace.cook_time as i16,
        }
        .into(),
        ContainerSetData {
            window_id,
            property: 3,
            value: furnace.cook_total as i16,
        }
        .into(),
    ]
}

type PendingChunk = (Arc<Chunk>, Arc<[u8]>);

#[derive(Debug)]
pub(super) struct CreativeSession {
    world: Arc<World>,
    pending_chunks: HashMap<[i32; 2], PendingChunk>,
    mining: Option<(BlockPos, tokio::time::Instant)>,
}

impl CreativeSession {
    pub(super) fn new(world: Arc<World>) -> Self {
        Self {
            world,
            pending_chunks: HashMap::new(),
            mining: None,
        }
    }

    pub(super) fn prepare_chunks(&mut self, packets: &mut [Clientbound]) {
        for packet in packets {
            if let Clientbound::ChunkData(data) = packet {
                let prepared = self.world.prepare_chunk(data.0);
                data.1 = prepared.payload;
                self.pending_chunks
                    .insert(data.0, (prepared.snapshot, prepared.light));
            }
        }
    }

    pub(super) fn chunk_sent(&mut self, slot: &Reservation, chunk: [i32; 2]) -> io::Result<()> {
        let (old, old_light) = self.pending_chunks.remove(&chunk).expect("prepared chunk");
        self.world
            .reconcile_and_subscribe(chunk, &old, &old_light, |changes, light_update| {
                let updates = encode_chunk_updates(chunk, changes, light_update)?;
                slot.watch_blocks(chunk, updates);
                Ok(())
            })
    }
}

type Bodies = Arc<[Arc<[u8]>]>;
fn encode_chunk_updates(
    chunk: [i32; 2],
    changes: Vec<(BlockPos, i32)>,
    light_update: Option<Arc<[u8]>>,
) -> io::Result<Bodies> {
    if changes.is_empty() && light_update.is_none() {
        return Ok(Arc::from([]));
    }
    let mut bodies = Vec::with_capacity(changes.len() + usize::from(light_update.is_some()));
    for (pos, state) in changes {
        bodies.push(Clientbound::from(BlockUpdate(pos, state)).encode()?.into());
    }
    if let Some(light) = light_update {
        bodies.push(
            Clientbound::from(LightUpdate(chunk, light))
                .encode()?
                .into(),
        );
    }
    Ok(bodies.into())
}

#[derive(Clone, Copy)]
enum Action {
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

fn interact(ctx: &mut Context<'_>, action: Action, sequence: i32) -> io::Result<Outcome> {
    let session = &mut *ctx.session;
    let state = session
        .play
        .as_mut()
        .ok_or_else(|| crate::protocol::invalid("Play session not initialized"))?;
    if !state.player.can_interact() || state.player.dead {
        return Ok(reply(vec![BlockAck(sequence).into()]));
    }
    let Some(slot) = session.reservation.as_ref() else {
        return Ok(reply(vec![BlockAck(sequence).into()]));
    };
    let is_survival = state.player.is_survival();
    let world = state.blocks.world.clone();
    let ack: Arc<[u8]> = Clientbound::from(BlockAck(sequence)).encode()?.into();

    // Handle Survival mining start/cancel without triggering block correction.
    if is_survival && let Action::Dig { pos, face, kind } = action {
        if kind == 1 {
            if let Some((mpos, _)) = state.blocks.mining.take() {
                let _ = ctx.server.players().broadcast_entity_packet(
                    &crate::protocol::packets::play::BlockDestruction {
                        entity_id: state.player.entity_id,
                        pos: mpos,
                        stage: 0xff,
                    }
                    .into(),
                );
            }
            slot.correct_blocks(Vec::new(), ack);
            return Ok(Outcome::empty());
        }
        if kind == 0 {
            let valid = pos.valid() && state.chunks.tracks(pos.chunk());
            let reachable = valid && state.player.in_reach(pos);
            let current = world.block_at(pos);
            if face < 6
                && reachable
                && current != crate::game::AIR_STATE
                && let Some(def) = crate::game::block_by_state(current)
                && def.hardness > 0.0
            {
                state.blocks.mining = Some((pos, tokio::time::Instant::now()));
                let _ = ctx.server.players().broadcast_entity_packet(
                    &crate::protocol::packets::play::BlockDestruction {
                        entity_id: state.player.entity_id,
                        pos,
                        stage: 0,
                    }
                    .into(),
                );
                slot.correct_blocks(Vec::new(), ack);
                return Ok(Outcome::empty());
            }
        }
    }

    let valid = |pos: BlockPos| pos.valid() && state.chunks.tracks(pos.chunk());
    let reachable = |pos: BlockPos| valid(pos) && state.player.in_reach(pos);
    let mut survival_dig_drops: Vec<(BlockPos, crate::game::ItemStack)> = Vec::new();
    let mut survival_tool_damage: u32 = 0;
    let mut survival_consume_hand: Option<Hand> = None;
    let mut extra_edits: Vec<(BlockPos, i32)> = Vec::new();
    let mut broken_chest_pos: Option<BlockPos> = None;
    let mut broken_chest_items: Vec<crate::game::ItemStack> = Vec::new();
    let mut clear_break_anim: Option<(i32, BlockPos)> = None;

    let (positions, edit) = match action {
        Action::Dig { pos, face, kind } => {
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
                        let tool_stack = session.inventory.hand(Hand::Main);
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
                            // Vanilla `Player.getDestroySpeed`: /5 when airborne or eyes underwater.
                            if !state.player.on_ground {
                                speed /= 5.0;
                            }
                            let eye = state.player.eye_position();
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
                            state.blocks.mining.take().is_some_and(|(mpos, started)| {
                                clear_break_anim = Some((state.player.entity_id, mpos));
                                mpos == pos && started.elapsed().as_secs_f64() >= expected * 0.7
                            })
                        } else {
                            false
                        };
                        if !allowed {
                            return None;
                        }
                        if state_def.hardness > 0.0 {
                            // Vanilla: swords take 2 durability damage when breaking blocks.
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
                if is_survival {
                    state.player.add_exhaustion(0.005);
                }
                // Drop any block-entity container contents via unified BlockBehavior.
                let drops = crate::game::block_behavior(current).container_drops(&world, pos);
                if !drops.is_empty()
                    || world.chest_items(pos).is_some()
                    || world.furnace_data(pos).is_some()
                {
                    broken_chest_pos = Some(pos);
                    broken_chest_items.extend(drops);
                }
                // Breaking either half of a bed, door, or double plant breaks its paired half.
                if let Some(other) = crate::game::paired_block_pos(pos, current)
                    && other.valid()
                    && world.block_at(other) != crate::game::AIR_STATE
                {
                    extra_edits.push((other, crate::game::AIR_STATE));
                }
                // Pop off any adjacent blocks that depended on `pos` for solid support.
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
        Action::Place {
            support,
            face,
            hand,
            hit,
        } => {
            let place = support.on_face(face);
            let edit = place.and_then(|pos| {
                let hand = Hand::from_wire(hand)?;
                let stack = session.inventory.hand(hand)?;
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
                        yaw: state.player.yaw,
                        pitch: state.player.pitch,
                        face,
                        hit,
                        support_state,
                        target_state,
                    },
                )?;
                let resolved = match placement {
                    crate::game::PlacementResult::MergeSupport(material) => {
                        let placed = crate::game::block_by_state(material)?;
                        (!placed.collision || !state.player.intersects_block(support))
                            .then_some((support, material))
                    }
                    crate::game::PlacementResult::PlaceTarget(mut material) => {
                        let placed = crate::game::block_by_state(material)?;
                        // Validate required solid support for attached blocks (torches, buttons, rails, etc.).
                        if let Some([sx, sy, sz]) = crate::game::required_support_offset(material) {
                            let sup_pos = BlockPos([pos.0[0] + sx, pos.0[1] + sy, pos.0[2] + sz]);
                            if !sup_pos.valid()
                                || !crate::game::is_solid_support(world.block_at(sup_pos))
                            {
                                return None;
                            }
                        }
                        // Compute vanilla stair corner shape from neighbors.
                        material =
                            crate::game::resolve_stairs_shape(pos, material, |p| world.block_at(p));
                        let target_ok = target_block.replaceable
                            || crate::game::registry()
                                .state_by_id(target_state)
                                .is_some_and(|(tb, _, _)| tb == block);
                        (reachable(pos)
                            && target_ok
                            && (!placed.collision || !state.player.intersects_block(pos)))
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
                            && (!p1.collision || !state.player.intersects_block(pos))
                            && (!p2.collision || !state.player.intersects_block(pos2));
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
        let _ = ctx.server.players().broadcast_entity_packet(
            &crate::protocol::packets::play::BlockDestruction {
                entity_id: eid,
                pos: mpos,
                stage: 0xff,
            }
            .into(),
        );
    }
    if state.pending_teleport.is_none()
        && let Some((pos, material)) = edit
        && let Some(mutation) = world.set_block(pos, material)
    {
        let chunk = pos.chunk();
        let mut center_light = None;
        for (nc, light) in mutation.light_changes {
            if nc == chunk {
                center_light = Some(light);
            } else {
                let body = Clientbound::from(LightUpdate(nc, light)).encode()?.into();
                slot.publish_light(nc, body);
            }
        }
        let data = encode_chunk_updates(chunk, mutation.block_changes, center_light)?;
        slot.publish_blocks(chunk, data, ack);

        // Apply any secondary edits (paired bed/door/double-plant halves, popped attached blocks).
        for (epos, estate) in extra_edits {
            let _ = ctx
                .server
                .play
                .set_block(ctx.server.players(), epos, estate);
        }
        // Recompute corner shapes of adjacent horizontal stairs if a stair was placed or broken.
        for [dx, dz] in [[1, 0], [-1, 0], [0, 1], [0, -1]] {
            let npos = BlockPos([pos.0[0] + dx, pos.0[1], pos.0[2] + dz]);
            if npos.valid() {
                let nstate = world.block_at(npos);
                let updated =
                    crate::game::resolve_stairs_shape(npos, nstate, |p| world.block_at(p));
                if updated != nstate {
                    let _ = ctx
                        .server
                        .play
                        .set_block(ctx.server.players(), npos, updated);
                }
            }
        }

        // Drop chest contents and close any open viewers if a chest was broken.
        if let Some(cpos) = broken_chest_pos {
            let drop_xyz = [
                f64::from(cpos.0[0]) + 0.5,
                f64::from(cpos.0[1]) + 0.5,
                f64::from(cpos.0[2]) + 0.5,
            ];
            for stack in broken_chest_items {
                let _ = ctx.server.play.spawn_item_drop_scatter(
                    ctx.server.players(),
                    drop_xyz,
                    stack,
                    10,
                );
            }
            for viewer in ctx.server.play.other_chest_viewers(cpos, uuid::Uuid::nil()) {
                ctx.server.players().send_control(
                    viewer,
                    crate::players::PlayerControl::RefreshContainer(cpos),
                );
            }
        }

        let mut inv_changed = false;
        if survival_tool_damage > 0 {
            inv_changed |= session
                .inventory
                .damage_hand(Hand::Main, survival_tool_damage)
                == crate::game::ClickResult::Changed;
        }
        if let Some(hand) = survival_consume_hand {
            inv_changed |=
                session.inventory.drop_hand(hand, false) == crate::game::ClickResult::Changed;
        }
        for (drop_pos, stack) in survival_dig_drops {
            let drop_xyz = [
                f64::from(drop_pos.0[0]) + 0.5,
                f64::from(drop_pos.0[1]) + 0.5,
                f64::from(drop_pos.0[2]) + 0.5,
            ];
            ctx.server
                .play
                .spawn_item_drop_scatter(ctx.server.players(), drop_xyz, stack, 10)?;
        }
        if inv_changed {
            sync_equipment(ctx)?;
            return inventory(ctx);
        }
        return Ok(Outcome::empty());
    }
    let mut corrections = Vec::new();
    for pos in positions.into_iter().flatten().filter(|&pos| valid(pos)) {
        let body = Clientbound::from(BlockUpdate(pos, world.block_at(pos)))
            .encode()?
            .into();
        corrections.push((pos.chunk(), body));
    }
    // Corrections cannot overtake earlier edits. An unload can discard
    // these block updates, but not the sequence acknowledgement.
    slot.correct_blocks(corrections, ack);
    Ok(Outcome::empty())
}

fn reply(replies: Vec<Clientbound>) -> Outcome {
    Outcome {
        replies,
        close: false,
        encryption: None,
    }
}

pub async fn handle_dig(ctx: &mut Context<'_>, p: PlayerDigging) -> io::Result<Outcome> {
    match p.action {
        0..=2 => interact(
            ctx,
            Action::Dig {
                pos: p.pos,
                face: p.face,
                kind: p.action,
            },
            p.sequence,
        ),
        3 | 4 => {
            let all = p.action == 3;
            let dropped = ctx.session.inventory.hand(Hand::Main).map(|mut s| {
                if !all {
                    s.count = 1;
                }
                s
            });
            if ctx.session.inventory.drop_hand(Hand::Main, all) == crate::game::ClickResult::Changed
                && let Some(stack) = dropped
            {
                let (eye, yaw, pitch) = {
                    let st = play(ctx.session)?;
                    (st.player.eye_position(), st.player.yaw, st.player.pitch)
                };
                let yaw_rad = f64::from(yaw).to_radians();
                let pitch_rad = f64::from(pitch).to_radians();
                let vx = -yaw_rad.sin() * pitch_rad.cos() * 0.3;
                let vy = -pitch_rad.sin() * 0.3 + 0.1;
                let vz = yaw_rad.cos() * pitch_rad.cos() * 0.3;
                ctx.server.play.spawn_item_drop(
                    ctx.server.players(),
                    eye,
                    [vx, vy, vz],
                    stack,
                    40,
                )?;
            }
            sync_equipment(ctx)?;
            inventory(ctx)
        }
        5 => {
            if let Ok(state) = play(ctx.session) {
                state.player.eating = None;
            }
            Ok(Outcome::empty())
        }
        6 => {
            ctx.session.inventory.swap_hands();
            sync_equipment(ctx)?;
            inventory(ctx)
        }
        _ => inventory(ctx),
    }
}

pub async fn handle_place(ctx: &mut Context<'_>, p: BlockPlace) -> io::Result<Outcome> {
    // Dispatch right-click interaction through unified `BlockBehavior::on_use`.
    let effects = if let Some(state) = ctx.session.play.as_mut()
        && state.player.can_interact()
        && !state.player.dead
        && !state.player.sneaking
        && state.pending_teleport.is_none()
        && state.player.in_reach(p.pos)
    {
        let world = ctx.server.play.world();
        let clicked_state = world.block_at(p.pos);
        let mut use_ctx = crate::game::UseContext {
            world,
            player: &mut state.player,
            inventory: &mut ctx.session.inventory,
        };
        crate::game::block_behavior(clicked_state).on_use(&mut use_ctx, p.pos, clicked_state)
    } else {
        crate::game::GameEffects::pass()
    };
    if effects.handled {
        return apply_game_effects(ctx, effects, p.sequence);
    }
    interact(
        ctx,
        Action::Place {
            support: p.pos,
            face: p.face,
            hand: p.hand,
            hit: p.hit,
        },
        p.sequence,
    )
}

/// Apply unified `GameEffects` emitted by any `BlockBehavior` or `ItemBehavior`
/// and translate them into protocol replies and broadcasts.
fn apply_game_effects(
    ctx: &mut Context<'_>,
    effects: crate::game::GameEffects,
    sequence: i32,
) -> io::Result<Outcome> {
    for (upos, ustate) in effects.block_updates {
        let _ = ctx
            .server
            .play
            .set_block(ctx.server.players(), upos, ustate)?;
    }
    if let Some(day_time) = effects.set_day_time {
        let time_pkt = ctx.server.play.set_day_time(day_time);
        let _ = ctx.server.players().broadcast(&time_pkt.into());
    }
    if let Some(duration) = effects.clear_weather_ticks {
        for wpkt in ctx.server.play.set_weather(false, false, duration) {
            let _ = ctx.server.players().broadcast(&wpkt);
        }
    }
    for msg in effects.broadcast_messages {
        let _ = ctx.server.players().broadcast_system_chat(&msg);
    }
    if !effects.player_drops.is_empty()
        && let Ok(state) = play(ctx.session)
    {
        let ppos = state.player.position;
        for stack in effects.player_drops {
            let _ = ctx.server.play.spawn_item_drop_scatter(
                ctx.server.players(),
                [ppos[0], ppos[1] + 0.5, ppos[2]],
                stack,
                10,
            );
        }
    }

    let mut replies: Vec<Clientbound> = effects
        .player_messages
        .into_iter()
        .map(|msg| crate::protocol::packets::play_chat::SystemChat(msg).into())
        .collect();

    if let Some(screen) = effects.open_screen {
        let world = ctx.server.play.world().clone();
        match screen {
            crate::game::ContainerScreen::CraftingTable => {
                let state = play(ctx.session)?;
                let window_id = state.next_window_id;
                state.next_window_id = if window_id >= 100 { 1 } else { window_id + 1 };
                state.open_container = Some(OpenContainer {
                    window_id,
                    kind: ContainerKind::CraftingTable {
                        grid: [None; 9],
                        output: None,
                    },
                    state_id: 0,
                });
                let slots = crate::game::containers::crafting_table_slots(
                    None,
                    &[None; 9],
                    &ctx.session.inventory,
                );
                replies.push(
                    OpenScreen {
                        window_id: i32::from(window_id),
                        menu_type: 12, // crafting
                        title: "Crafting".into(),
                    }
                    .into(),
                );
                replies.push(
                    InventoryContent::container(
                        window_id,
                        0,
                        slots,
                        ctx.session.inventory.carried(),
                    )
                    .into(),
                );
            }
            crate::game::ContainerScreen::Furnace(fpos) => {
                if let Some(furnace) = world.furnace_data(fpos) {
                    let state = play(ctx.session)?;
                    let uuid = state.player.uuid;
                    let window_id = state.next_window_id;
                    state.next_window_id = if window_id >= 100 { 1 } else { window_id + 1 };
                    state.open_container = Some(OpenContainer {
                        window_id,
                        kind: ContainerKind::Furnace(fpos),
                        state_id: 0,
                    });
                    let _ = ctx.server.play.open_chest_viewer(fpos, uuid);
                    let slots =
                        crate::game::containers::furnace_slots(&furnace, &ctx.session.inventory);
                    replies.push(
                        OpenScreen {
                            window_id: i32::from(window_id),
                            menu_type: 14, // furnace
                            title: "Furnace".into(),
                        }
                        .into(),
                    );
                    replies.push(
                        InventoryContent::container(
                            window_id,
                            0,
                            slots,
                            ctx.session.inventory.carried(),
                        )
                        .into(),
                    );
                    replies.extend(furnace_property_packets(window_id, &furnace));
                }
            }
            crate::game::ContainerScreen::Chest(cpos) => {
                if let Some(chest_items) = world.chest_items(cpos) {
                    let state = play(ctx.session)?;
                    let uuid = state.player.uuid;
                    let window_id = state.next_window_id;
                    state.next_window_id = if window_id >= 100 { 1 } else { window_id + 1 };
                    state.open_container = Some(OpenContainer {
                        window_id,
                        kind: ContainerKind::Chest(cpos),
                        state_id: 0,
                    });
                    let viewer_count = ctx.server.play.open_chest_viewer(cpos, uuid);
                    broadcast_chest_lid_count(ctx.server.players(), cpos, viewer_count)?;
                    let slots = crate::game::containers::container_slots(
                        &chest_items,
                        &ctx.session.inventory,
                    );
                    replies.push(
                        OpenScreen {
                            window_id: i32::from(window_id),
                            menu_type: 2, // generic_9x3
                            title: "Chest".into(),
                        }
                        .into(),
                    );
                    replies.push(
                        InventoryContent::container(
                            window_id,
                            0,
                            slots,
                            ctx.session.inventory.carried(),
                        )
                        .into(),
                    );
                }
            }
        }
    }

    if effects.inventory_changed {
        sync_equipment(ctx)?;
        replies.push(InventoryContent::player(&ctx.session.inventory).into());
    }
    if replies.is_empty()
        && let Some(slot) = &ctx.session.reservation
        && ctx
            .session
            .play
            .as_ref()
            .is_some_and(|state| state.player.can_interact())
    {
        let ack = Clientbound::from(BlockAck(sequence)).encode()?.into();
        slot.correct_blocks(Vec::new(), ack);
        return Ok(Outcome::empty());
    }
    replies.push(BlockAck(sequence).into());
    Ok(reply(replies))
}

pub async fn handle_close_window(ctx: &mut Context<'_>, p: CloseWindow) -> io::Result<Outcome> {
    let (uuid, pos, closed) = {
        let state = play(ctx.session)?;
        let closed = if let Some(open) = &state.open_container
            && (p.window_id == 0 || open.window_id == p.window_id)
        {
            state.open_container.take()
        } else {
            None
        };
        (state.player.uuid, state.player.position, closed)
    };
    let mut returned = Vec::new();
    if let Some(open) = closed {
        match open.kind {
            ContainerKind::Chest(chest_pos) => {
                let count = ctx.server.play.close_chest_viewer(chest_pos, uuid);
                broadcast_chest_lid_count(ctx.server.players(), chest_pos, count)?;
            }
            ContainerKind::Furnace(furnace_pos) => {
                let _ = ctx.server.play.close_chest_viewer(furnace_pos, uuid);
            }
            ContainerKind::CraftingTable { grid, .. } => {
                returned.extend(grid.into_iter().flatten());
            }
        }
    }
    if p.window_id == 0 {
        returned.extend(ctx.session.inventory.drain_crafting_grid());
    }
    if let Some(carried) = ctx.session.inventory.take_carried() {
        returned.push(carried);
    }
    if !returned.is_empty() {
        for mut stack in returned {
            let added = ctx.session.inventory.give_stack(stack);
            if added < stack.count {
                stack.count -= added;
                let _ = ctx.server.play.spawn_item_drop_scatter(
                    ctx.server.players(),
                    [pos[0], pos[1] + 0.5, pos[2]],
                    stack,
                    40,
                );
            }
        }
        sync_equipment(ctx)?;
        return inventory(ctx);
    }
    Ok(Outcome::empty())
}

pub(super) fn broadcast_chest_lid_count(
    players: &crate::players::Players,
    pos: BlockPos,
    viewer_count: usize,
) -> io::Result<()> {
    let Some(chest_block_id) = crate::game::registry()
        .block_by_name("chest")
        .map(|(id, _)| id as i32)
    else {
        return Ok(());
    };
    let body: Arc<[u8]> = Clientbound::from(BlockAction {
        pos,
        action_id: 1,
        action_param: viewer_count.min(255) as u8,
        block_id: chest_block_id,
    })
    .encode()?
    .into();
    players.publish_world_updates(pos.chunk(), Arc::from([body]));
    Ok(())
}

pub async fn handle_use_item(ctx: &mut Context<'_>, p: UseItem) -> io::Result<Outcome> {
    let effects = if let Some(hand) = Hand::from_wire(p.hand)
        && let Some(stack) = ctx.session.inventory.hand(hand)
        && let Some(state) = ctx.session.play.as_mut()
    {
        let world = ctx.server.play.world();
        let mut use_ctx = crate::game::UseContext {
            world,
            player: &mut state.player,
            inventory: &mut ctx.session.inventory,
        };
        crate::game::item_behavior(stack.item).on_use(&mut use_ctx, hand, stack)
    } else {
        crate::game::GameEffects::pass()
    };
    apply_game_effects(ctx, effects, p.sequence)
}

pub async fn handle_slot(ctx: &mut Context<'_>, p: HeldItemSlot) -> io::Result<Outcome> {
    let state = play(ctx.session)?;
    state.player.eating = None;
    ctx.session.inventory.select(p.slot as u8);
    sync_equipment(ctx)?;
    Ok(Outcome::empty())
}

pub(super) fn sync_equipment_public(ctx: &mut Context<'_>) -> io::Result<()> {
    sync_equipment(ctx)
}

fn sync_equipment(ctx: &mut Context<'_>) -> io::Result<()> {
    let entity_id = play(ctx.session)?.player.entity_id;
    let Some(slot) = &ctx.session.reservation else {
        return Ok(());
    };
    let packet = Clientbound::from(crate::protocol::packets::play_entities::EntityEquipment {
        id: entity_id,
        main_hand: ctx.session.inventory.hand(Hand::Main),
        off_hand: ctx.session.inventory.hand(Hand::Off),
    });
    slot.broadcast_to_viewers(&packet)
}

fn inventory(ctx: &mut Context<'_>) -> io::Result<Outcome> {
    if play(ctx.session)?.player.can_interact() {
        Ok(reply(vec![
            InventoryContent::player(&ctx.session.inventory).into(),
        ]))
    } else {
        Ok(Outcome::empty())
    }
}

pub async fn handle_creative_slot(ctx: &mut Context<'_>, p: CreativeSlot) -> io::Result<Outcome> {
    if !play(ctx.session)?.player.is_creative() {
        return Ok(Outcome::empty());
    }
    if let Some(item) = p.item.supported() {
        ctx.session.inventory.set_creative(p.slot, item);
        sync_equipment(ctx)?;
    }
    // Reply with only the affected slot instead of full InventoryContent;
    // Creative inventory keeps the cursor client-side, and sending
    // InventoryContent with an empty carried item would wipe it out.
    let Ok(slot) = usize::try_from(p.slot) else {
        return Ok(Outcome::empty());
    };
    let Some(&item) = ctx.session.inventory.slots().get(slot) else {
        return Ok(Outcome::empty());
    };
    Ok(reply(vec![
        SetInventorySlot {
            state_id: ctx.session.inventory.state_id(),
            slot: p.slot,
            item,
        }
        .into(),
    ]))
}
fn throw_item_from_player(ctx: &mut Context<'_>, stack: crate::game::ItemStack) -> io::Result<()> {
    let (eye, yaw, pitch) = {
        let st = play(ctx.session)?;
        (st.player.eye_position(), st.player.yaw, st.player.pitch)
    };
    let yaw_rad = f64::from(yaw).to_radians();
    let pitch_rad = f64::from(pitch).to_radians();
    let vx = -yaw_rad.sin() * pitch_rad.cos() * 0.3;
    let vy = -pitch_rad.sin() * 0.3 + 0.1;
    let vz = yaw_rad.cos() * pitch_rad.cos() * 0.3;
    ctx.server
        .play
        .spawn_item_drop(ctx.server.players(), eye, [vx, vy, vz], stack, 40)
}

pub async fn handle_inventory_click(
    ctx: &mut Context<'_>,
    p: InventoryClickPacket,
) -> io::Result<Outcome> {
    if !play(ctx.session)?.player.can_interact() {
        return Ok(Outcome::empty());
    }
    if p.window != 0 {
        let (open, uuid) = {
            let state = play(ctx.session)?;
            let Some(open) = state.open_container.clone() else {
                return Ok(Outcome::empty());
            };
            (open, state.player.uuid)
        };
        if open.window_id != p.window {
            return Ok(Outcome::empty());
        }
        let next_state_id = open.state_id.wrapping_add(1);
        match open.kind {
            ContainerKind::Chest(cpos) => {
                let world = ctx.server.play.world().clone();
                let inv = &mut ctx.session.inventory;
                let mut dropped: Option<crate::game::ItemStack> = None;
                let updated = world.update_chest(cpos, |chest| {
                    let (changed, drop) =
                        crate::game::containers::apply_chest_click(chest, inv, p.click);
                    dropped = drop;
                    changed
                });
                let Some(chest_items) = updated else {
                    play(ctx.session)?.open_container = None;
                    return Ok(reply(vec![CloseContainer(open.window_id).into()]));
                };
                if let Some(stack) = dropped {
                    throw_item_from_player(ctx, stack)?;
                }
                for other in ctx.server.play.other_chest_viewers(cpos, uuid) {
                    ctx.server
                        .players()
                        .send_control(other, crate::players::PlayerControl::RefreshContainer(cpos));
                }
                play(ctx.session)?.open_container = Some(OpenContainer {
                    window_id: open.window_id,
                    kind: ContainerKind::Chest(cpos),
                    state_id: next_state_id,
                });
                sync_equipment(ctx)?;
                let slots =
                    crate::game::containers::container_slots(&chest_items, &ctx.session.inventory);
                return Ok(reply(vec![
                    InventoryContent::container(
                        open.window_id,
                        next_state_id,
                        slots,
                        ctx.session.inventory.carried(),
                    )
                    .into(),
                ]));
            }
            ContainerKind::CraftingTable {
                mut grid,
                mut output,
            } => {
                let dropped = crate::game::containers::apply_crafting_table_click(
                    &mut grid,
                    &mut output,
                    &mut ctx.session.inventory,
                    p.click,
                );
                if let Some(stack) = dropped {
                    throw_item_from_player(ctx, stack)?;
                }
                play(ctx.session)?.open_container = Some(OpenContainer {
                    window_id: open.window_id,
                    kind: ContainerKind::CraftingTable { grid, output },
                    state_id: next_state_id,
                });
                sync_equipment(ctx)?;
                let slots = crate::game::containers::crafting_table_slots(
                    output,
                    &grid,
                    &ctx.session.inventory,
                );
                return Ok(reply(vec![
                    InventoryContent::container(
                        open.window_id,
                        next_state_id,
                        slots,
                        ctx.session.inventory.carried(),
                    )
                    .into(),
                ]));
            }
            ContainerKind::Furnace(fpos) => {
                let world = ctx.server.play.world().clone();
                let inv = &mut ctx.session.inventory;
                let mut dropped: Option<crate::game::ItemStack> = None;
                let updated = world.update_furnace(fpos, |furnace| {
                    let (changed, drop) =
                        crate::game::containers::apply_furnace_click(furnace, inv, p.click);
                    dropped = drop;
                    changed
                });
                let Some(furnace) = updated else {
                    play(ctx.session)?.open_container = None;
                    return Ok(reply(vec![CloseContainer(open.window_id).into()]));
                };
                if let Some(stack) = dropped {
                    throw_item_from_player(ctx, stack)?;
                }
                for other in ctx.server.play.other_chest_viewers(fpos, uuid) {
                    ctx.server
                        .players()
                        .send_control(other, crate::players::PlayerControl::RefreshContainer(fpos));
                }
                play(ctx.session)?.open_container = Some(OpenContainer {
                    window_id: open.window_id,
                    kind: ContainerKind::Furnace(fpos),
                    state_id: next_state_id,
                });
                sync_equipment(ctx)?;
                let slots =
                    crate::game::containers::furnace_slots(&furnace, &ctx.session.inventory);
                let mut replies = vec![
                    InventoryContent::container(
                        open.window_id,
                        next_state_id,
                        slots,
                        ctx.session.inventory.carried(),
                    )
                    .into(),
                ];
                replies.extend(furnace_property_packets(open.window_id, &furnace));
                return Ok(reply(replies));
            }
        }
    }
    if p.window == 0 && p.state_id == ctx.session.inventory.state_id() {
        let dropped = crate::game::containers::preview_player_drop(&ctx.session.inventory, p.click);
        if ctx.session.inventory.click(p.click) == crate::game::ClickResult::Changed {
            if let Some(stack) = dropped {
                throw_item_from_player(ctx, stack)?;
            }
            sync_equipment(ctx)?;
        }
    }
    // Full authoritative state keeps unsupported modes and stale client state
    // simple and makes adding containers later independent of player storage.
    inventory(ctx)
}
