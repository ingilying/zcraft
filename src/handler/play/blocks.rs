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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OpenContainer {
    pub window_id: u8,
    pub pos: BlockPos,
    pub state_id: i32,
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
            state.blocks.mining = None;
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
                slot.correct_blocks(Vec::new(), ack);
                return Ok(Outcome::empty());
            }
        }
    }

    let valid = |pos: BlockPos| pos.valid() && state.chunks.tracks(pos.chunk());
    let reachable = |pos: BlockPos| valid(pos) && state.player.in_reach(pos);
    let mut survival_dig_drop: Option<(BlockPos, crate::game::ItemStack)> = None;
    let mut survival_damage_tool = false;
    let mut survival_consume_hand: Option<Hand> = None;

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
                    |(block_idx, block_def, state_def)| {
                        if state_def.hardness < 0.0 {
                            return None;
                        }
                        let tool_item = session.inventory.hand(Hand::Main).map(|s| s.item);
                        let can_harvest = !state_def.correct_tool
                            || crate::game::registry().correct_for_drops(tool_item, block_idx);
                        let allowed = if state_def.hardness == 0.0 {
                            kind == 0 || kind == 2
                        } else if kind == 2 {
                            let speed = if can_harvest {
                                crate::game::registry()
                                    .tool_speed(tool_item, block_idx)
                                    .max(1.0)
                            } else {
                                1.0
                            };
                            let expected = f64::from(state_def.hardness)
                                * if can_harvest { 1.5 } else { 5.0 }
                                / f64::from(speed);
                            state.blocks.mining.take().is_some_and(|(mpos, started)| {
                                mpos == pos && started.elapsed().as_secs_f64() >= expected * 0.7
                            })
                        } else {
                            false
                        };
                        if !allowed {
                            return None;
                        }
                        if state_def.hardness > 0.0 {
                            survival_damage_tool = true;
                        }
                        if can_harvest
                            && let Some(item_name) = block_def.item.as_deref()
                            && let Some(item_def) = crate::game::item_by_name(item_name)
                            && let Some(stack) = crate::game::ItemStack::new(item_def.id, 1)
                        {
                            survival_dig_drop = Some((pos, stack));
                        }
                        Some((pos, crate::game::AIR_STATE))
                    },
                )
            } else {
                None
            };
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
                    crate::game::PlacementResult::PlaceTarget(material) => {
                        let placed = crate::game::block_by_state(material)?;
                        let target_ok = target_block.replaceable
                            || crate::game::registry()
                                .state_by_id(target_state)
                                .is_some_and(|(tb, _, _)| tb == block);
                        (reachable(pos)
                            && target_ok
                            && (!placed.collision || !state.player.intersects_block(pos)))
                        .then_some((pos, material))
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

        let mut inv_changed = false;
        if survival_damage_tool {
            inv_changed |=
                session.inventory.damage_hand(Hand::Main) == crate::game::ClickResult::Changed;
        }
        if let Some(hand) = survival_consume_hand {
            inv_changed |=
                session.inventory.drop_hand(hand, false) == crate::game::ClickResult::Changed;
        }
        if let Some((drop_pos, stack)) = survival_dig_drop {
            let drop_xyz = [
                f64::from(drop_pos.0[0]) + 0.5,
                f64::from(drop_pos.0[1]) + 0.5,
                f64::from(drop_pos.0[2]) + 0.5,
            ];
            ctx.server.play.spawn_item_drop(
                ctx.server.players(),
                drop_xyz,
                [0.0, 0.2, 0.0],
                stack,
                10,
            )?;
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
        5 => Ok(Outcome::empty()), // Release use-item; no continuous actions exist yet.
        6 => {
            ctx.session.inventory.swap_hands();
            sync_equipment(ctx)?;
            inventory(ctx)
        }
        _ => inventory(ctx),
    }
}

pub async fn handle_place(ctx: &mut Context<'_>, p: BlockPlace) -> io::Result<Outcome> {
    // Right-clicking a chest while not sneaking opens its 27-slot container.
    if let Ok(state) = play(ctx.session)
        && state.player.can_interact()
        && !state.player.dead
        && !state.player.sneaking
        && state.pending_teleport.is_none()
        && state.player.in_reach(p.pos)
        && let Some(chest_items) = ctx.server.play.world().chest_items(p.pos)
    {
        let window_id = state.next_window_id;
        state.next_window_id = if window_id >= 100 { 1 } else { window_id + 1 };
        let open = OpenContainer {
            window_id,
            pos: p.pos,
            state_id: 0,
        };
        state.open_container = Some(open);
        broadcast_chest_lid(ctx, p.pos, true)?;
        let slots = container_slots(&chest_items, &ctx.session.inventory);
        return Ok(reply(vec![
            OpenScreen {
                window_id: i32::from(window_id),
                menu_type: 2, // generic_9x3
                title: "Chest".into(),
            }
            .into(),
            InventoryContent::container(window_id, 0, slots, ctx.session.inventory.carried())
                .into(),
            BlockAck(p.sequence).into(),
        ]));
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

pub async fn handle_close_window(ctx: &mut Context<'_>, p: CloseWindow) -> io::Result<Outcome> {
    let state = play(ctx.session)?;
    if let Some(open) = state.open_container
        && (p.window_id == 0 || open.window_id == p.window_id)
    {
        state.open_container = None;
        broadcast_chest_lid(ctx, open.pos, false)?;
    }
    Ok(Outcome::empty())
}

fn broadcast_chest_lid(ctx: &Context<'_>, pos: BlockPos, open: bool) -> io::Result<()> {
    let Some(chest_block_id) = crate::game::registry()
        .block_by_name("chest")
        .map(|(id, _)| id as i32)
    else {
        return Ok(());
    };
    let body: Arc<[u8]> = Clientbound::from(BlockAction {
        pos,
        action_id: 1,
        action_param: u8::from(open),
        block_id: chest_block_id,
    })
    .encode()?
    .into();
    ctx.server
        .players()
        .publish_world_updates(pos.chunk(), Arc::from([body]));
    Ok(())
}

fn container_slots(
    chest: &[Option<crate::game::ItemStack>; crate::world::CHEST_SLOTS],
    inv: &crate::game::Inventory,
) -> Vec<Option<crate::game::ItemStack>> {
    let mut slots = Vec::with_capacity(crate::world::CHEST_SLOTS + 36);
    slots.extend_from_slice(chest);
    slots.extend_from_slice(&inv.slots()[9..45]);
    slots
}

pub async fn handle_use_item(ctx: &mut Context<'_>, p: UseItem) -> io::Result<Outcome> {
    let _held = Hand::from_wire(p.hand).and_then(|hand| ctx.session.inventory.hand(hand));
    if let Some(slot) = &ctx.session.reservation
        && ctx
            .session
            .play
            .as_ref()
            .is_some_and(|state| state.player.can_interact())
    {
        // Future food/tools/projectiles dispatch through item categories here. Even a
        // no-op use is acknowledged in the same FIFO as block predictions.
        let ack = Clientbound::from(BlockAck(p.sequence)).encode()?.into();
        slot.correct_blocks(Vec::new(), ack);
        return Ok(Outcome::empty());
    }
    Ok(reply(vec![BlockAck(p.sequence).into()]))
}

pub async fn handle_slot(ctx: &mut Context<'_>, p: HeldItemSlot) -> io::Result<Outcome> {
    play(ctx.session)?;
    ctx.session.inventory.select(p.slot as u8);
    sync_equipment(ctx)?;
    Ok(Outcome::empty())
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
pub async fn handle_inventory_click(
    ctx: &mut Context<'_>,
    p: InventoryClickPacket,
) -> io::Result<Outcome> {
    if !play(ctx.session)?.player.can_interact() {
        return Ok(Outcome::empty());
    }
    if p.window != 0 {
        let Some(open) = play(ctx.session)?.open_container else {
            return Ok(Outcome::empty());
        };
        if open.window_id != p.window {
            return Ok(Outcome::empty());
        }
        let world = ctx.server.play.world().clone();
        let inv = &mut ctx.session.inventory;
        let updated = world.update_chest(open.pos, |chest| apply_chest_click(chest, inv, p.click));
        let Some(chest_items) = updated else {
            play(ctx.session)?.open_container = None;
            return Ok(reply(vec![CloseContainer(open.window_id).into()]));
        };
        let state = play(ctx.session)?;
        let next_state_id = open.state_id.wrapping_add(1);
        state.open_container = Some(OpenContainer {
            state_id: next_state_id,
            ..open
        });
        sync_equipment(ctx)?;
        let slots = container_slots(&chest_items, &ctx.session.inventory);
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
    if p.window == 0 && p.state_id == ctx.session.inventory.state_id() {
        let _ = ctx.session.inventory.click(p.click);
        sync_equipment(ctx)?;
    }
    // Full authoritative state keeps unsupported modes and stale client state
    // simple and makes adding containers later independent of player storage.
    inventory(ctx)
}

fn apply_chest_click(
    chest: &mut [Option<crate::game::ItemStack>; crate::world::CHEST_SLOTS],
    inv: &mut crate::game::Inventory,
    click: crate::game::InventoryClick,
) -> bool {
    let Ok(slot) = usize::try_from(click.slot) else {
        return false;
    };
    let total = crate::world::CHEST_SLOTS + 36;
    if slot >= total {
        return false;
    }
    // Shift-click (mode 1): quick-move between chest (0..27) and player inventory (27..63).
    if click.mode == 1 {
        let stack = if slot < crate::world::CHEST_SLOTS {
            chest[slot]
        } else {
            inv.slots()[9 + (slot - crate::world::CHEST_SLOTS)]
        };
        let Some(mut moving) = stack else {
            return false;
        };
        let max_stack = moving.definition().max_stack;
        if slot < crate::world::CHEST_SLOTS {
            for target in (9..45).rev() {
                if let Some(mut existing) = inv.slots()[target]
                    && existing.item == moving.item
                    && existing.damage == moving.damage
                    && existing.count < max_stack
                {
                    let add = (max_stack - existing.count).min(moving.count);
                    existing.count += add;
                    moving.count -= add;
                    inv.set_slot(target, Some(existing));
                    if moving.count == 0 {
                        chest[slot] = None;
                        return true;
                    }
                }
            }
            for target in (9..45).rev() {
                if inv.slots()[target].is_none() {
                    inv.set_slot(target, Some(moving));
                    chest[slot] = None;
                    return true;
                }
            }
            chest[slot] = Some(moving);
            return true;
        } else {
            let inv_idx = 9 + (slot - crate::world::CHEST_SLOTS);
            for slot_ref in chest.iter_mut() {
                if let Some(mut existing) = *slot_ref
                    && existing.item == moving.item
                    && existing.damage == moving.damage
                    && existing.count < max_stack
                {
                    let add = (max_stack - existing.count).min(moving.count);
                    existing.count += add;
                    moving.count -= add;
                    *slot_ref = Some(existing);
                    if moving.count == 0 {
                        inv.set_slot(inv_idx, None);
                        return true;
                    }
                }
            }
            for slot_ref in chest.iter_mut() {
                if slot_ref.is_none() {
                    *slot_ref = Some(moving);
                    inv.set_slot(inv_idx, None);
                    return true;
                }
            }
            inv.set_slot(inv_idx, Some(moving));
            return true;
        }
    }

    if click.mode != 0 || !matches!(click.button, 0 | 1) {
        return false;
    }

    let current = if slot < crate::world::CHEST_SLOTS {
        chest[slot]
    } else {
        inv.slots()[9 + (slot - crate::world::CHEST_SLOTS)]
    };
    let carried = inv.carried();

    let (next_slot, next_carried) = match (click.button, current, carried) {
        // Left click: pick up / put down / merge / swap.
        (0, Some(mut cur), Some(mut car)) if cur.item == car.item && cur.damage == car.damage => {
            let max_stack = cur.definition().max_stack;
            let add = (max_stack - cur.count).min(car.count);
            cur.count += add;
            car.count -= add;
            (Some(cur), (car.count > 0).then_some(car))
        }
        (0, cur, car) => (car, cur),
        // Right click: split half when empty hand, or place 1 item.
        (1, Some(mut cur), None) => {
            let take = cur.count.div_ceil(2);
            cur.count -= take;
            let taken = crate::game::ItemStack::with_damage(cur.item, take, cur.damage);
            ((cur.count > 0).then_some(cur), taken)
        }
        (1, None, Some(mut car)) => {
            let placed = crate::game::ItemStack::with_damage(car.item, 1, car.damage);
            car.count -= 1;
            (placed, (car.count > 0).then_some(car))
        }
        (1, Some(mut cur), Some(mut car))
            if cur.item == car.item
                && cur.damage == car.damage
                && cur.count < cur.definition().max_stack =>
        {
            cur.count += 1;
            car.count -= 1;
            (Some(cur), (car.count > 0).then_some(car))
        }
        (1, cur, car) => (car, cur),
        _ => return false,
    };

    if slot < crate::world::CHEST_SLOTS {
        chest[slot] = next_slot;
    } else {
        inv.set_slot(9 + (slot - crate::world::CHEST_SLOTS), next_slot);
    }
    inv.set_carried(next_carried);
    true
}
