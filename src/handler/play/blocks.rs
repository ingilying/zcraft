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
    world::{self, BlockPos, BlockWorld, Snapshot},
};
use std::{collections::HashMap, io, sync::Arc};

#[derive(Debug)]
pub(super) struct CreativeSession {
    world: Arc<BlockWorld>,
    pending_chunks: HashMap<[i32; 2], (Snapshot, Arc<[u8]>)>,
}

impl CreativeSession {
    pub(super) fn new(world: Arc<BlockWorld>) -> Self {
        Self {
            world,
            pending_chunks: HashMap::new(),
        }
    }

    pub(super) fn prepare_chunks(&mut self, packets: &mut [Clientbound]) {
        for packet in packets {
            if let Clientbound::ChunkData(data) = packet {
                let neighborhood = self.world.lock().neighborhood(data.0);
                let light = world::chunk_light(&neighborhood);
                if world::has_any_edits(&neighborhood) {
                    data.1 = world::chunk_payload_with_light(&neighborhood);
                }
                let center = neighborhood[1][1].clone();
                self.pending_chunks.insert(data.0, (center, light));
            }
        }
    }

    pub(super) fn chunk_sent(&mut self, slot: &Reservation, chunk: [i32; 2]) -> io::Result<()> {
        let (old, old_light) = self.pending_chunks.remove(&chunk).expect("prepared chunk");
        let blocks = self.world.lock();
        let neighborhood = blocks.neighborhood(chunk);
        let new = &neighborhood[1][1];
        let changes = world::changed_blocks(chunk, &old, new);
        let new_light = world::chunk_light(&neighborhood);
        let updates = updates(chunk, changes, &old_light, new_light)?;
        // The edit lock spans both reconciliation and subscription. A write
        // can never slip in between the two; no lock spans a socket write.
        slot.watch_blocks(chunk, updates);
        Ok(())
    }
}

type Bodies = Arc<[Arc<[u8]>]>;
fn updates(
    chunk: [i32; 2],
    changes: Vec<(BlockPos, i32)>,
    old_light: &Arc<[u8]>,
    new_light: Arc<[u8]>,
) -> io::Result<Bodies> {
    let light_changed = new_light != *old_light;
    if changes.is_empty() && !light_changed {
        return Ok(Arc::from([]));
    }
    let mut bodies = Vec::with_capacity(changes.len() + usize::from(light_changed));
    for (pos, state) in changes {
        bodies.push(Clientbound::from(BlockUpdate(pos, state)).encode()?.into());
    }
    if light_changed {
        bodies.push(
            Clientbound::from(LightUpdate(chunk, new_light))
                .encode()?
                .into(),
        );
    }
    Ok(bodies.into())
}

fn in_reach(position: [f64; 3], pos: BlockPos) -> bool {
    let eye = [position[0], position[1] + 1.62, position[2]];
    eye.into_iter()
        .zip(pos.0)
        .map(|(p, b)| {
            let b = f64::from(b);
            (p - p.clamp(b, b + 1.)).powi(2)
        })
        .sum::<f64>()
        <= 5.0f64.powi(2)
}

fn intersects_player([x, y, z]: [f64; 3], pos: BlockPos) -> bool {
    let [bx, by, bz] = pos.0.map(f64::from);
    x + 0.3 > bx
        && x - 0.3 < bx + 1.
        && y + 1.8 > by
        && y < by + 1.
        && z + 0.3 > bz
        && z - 0.3 < bz + 1.
}

#[derive(Clone, Copy)]
enum Action {
    Dig {
        pos: BlockPos,
        face: u8,
        start: bool,
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
        .as_ref()
        .ok_or_else(|| crate::protocol::invalid("Play session not initialized"))?;
    if state.game_mode != crate::config::GameMode::Creative {
        return Ok(reply(vec![BlockAck(sequence).into()]));
    }
    let Some(slot) = session.reservation.as_ref() else {
        return Ok(reply(vec![BlockAck(sequence).into()]));
    };
    let valid = |pos: BlockPos| pos.valid() && state.chunks.tracks(pos.chunk());
    let reachable = |pos: BlockPos| valid(pos) && in_reach(state.position, pos);
    let mut blocks = state.blocks.world.lock();
    let (positions, edit) = match action {
        Action::Dig { pos, face, start } => {
            let current = blocks.get(pos);
            let edit = if start && face < 6 && reachable(pos) && current != crate::game::AIR_STATE {
                Some((pos, crate::game::AIR_STATE))
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
                let material = crate::game::registry().default_state(block)?;
                let support_block = crate::game::block_by_state(blocks.get(support))?;
                let target_block = crate::game::block_by_state(blocks.get(pos))?;
                let placed = crate::game::block_by_state(material)?;
                (hit.iter().all(|f| (0.0..=1.0).contains(f))
                    && reachable(support)
                    && reachable(pos)
                    && !support_block.replaceable
                    && target_block.replaceable
                    && (!placed.collision || !intersects_player(state.position, pos)))
                .then_some((pos, material))
            });
            ([Some(support), place], edit)
        }
    };
    let ack: Arc<[u8]> = Clientbound::from(BlockAck(sequence)).encode()?.into();
    if state.pending_teleport.is_none()
        && let Some((pos, material)) = edit
    {
        let chunk = pos.chunk();
        let affected = world::affected_light_chunks(pos);
        let old_lights: Vec<_> = affected
            .iter()
            .map(|&nc| (nc, world::chunk_light(&blocks.neighborhood(nc))))
            .collect();
        if blocks.set(pos, material) {
            for (nc, old_light) in old_lights {
                let new_light = world::chunk_light(&blocks.neighborhood(nc));
                if nc == chunk {
                    let data = updates(chunk, vec![(pos, material)], &old_light, new_light)?;
                    slot.publish_blocks(chunk, data, ack.clone());
                } else if new_light != old_light {
                    let body = Clientbound::from(LightUpdate(nc, new_light))
                        .encode()?
                        .into();
                    slot.publish_light(nc, body);
                }
            }
            return Ok(Outcome::empty());
        }
    }
    let mut corrections = Vec::new();
    for pos in positions.into_iter().flatten().filter(|&pos| valid(pos)) {
        let body = Clientbound::from(BlockUpdate(pos, blocks.get(pos)))
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
                start: p.action == 0,
            },
            p.sequence,
        ),
        3 => {
            ctx.session.inventory.drop_hand(Hand::Main, true);
            inventory(ctx)
        }
        4 => {
            ctx.session.inventory.drop_hand(Hand::Main, false);
            inventory(ctx)
        }
        5 => Ok(Outcome::empty()), // Release use-item; no continuous actions exist yet.
        6 => {
            ctx.session.inventory.swap_hands();
            inventory(ctx)
        }
        _ => inventory(ctx),
    }
}

pub async fn handle_place(ctx: &mut Context<'_>, p: BlockPlace) -> io::Result<Outcome> {
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

pub async fn handle_use_item(ctx: &mut Context<'_>, p: UseItem) -> io::Result<Outcome> {
    let _held = Hand::from_wire(p.hand).and_then(|hand| ctx.session.inventory.hand(hand));
    if let Some(slot) = &ctx.session.reservation
        && ctx
            .session
            .play
            .as_ref()
            .is_some_and(|state| state.game_mode == crate::config::GameMode::Creative)
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
    Ok(Outcome::empty())
}

fn inventory(ctx: &mut Context<'_>) -> io::Result<Outcome> {
    if play(ctx.session)?.game_mode == crate::config::GameMode::Creative {
        Ok(reply(vec![
            InventoryContent(Box::new(ctx.session.inventory.clone())).into(),
        ]))
    } else {
        Ok(Outcome::empty())
    }
}

pub async fn handle_creative_slot(ctx: &mut Context<'_>, p: CreativeSlot) -> io::Result<Outcome> {
    if play(ctx.session)?.game_mode != crate::config::GameMode::Creative {
        return Ok(Outcome::empty());
    }
    if let Some(item) = p.item.supported() {
        ctx.session.inventory.set_creative(p.slot, item);
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
    if play(ctx.session)?.game_mode != crate::config::GameMode::Creative {
        return Ok(Outcome::empty());
    }
    if p.window == 0 && p.state_id == ctx.session.inventory.state_id() {
        let _ = ctx.session.inventory.click(p.click);
    }
    // Full authoritative state keeps unsupported modes and stale client state
    // simple and makes adding containers later independent of player storage.
    inventory(ctx)
}
