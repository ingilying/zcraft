//! Server-authoritative interactions for the fixed creative hotbar.
#[cfg(test)]
#[path = "blocks_tests.rs"]
mod tests;
use super::{Context, Outcome, play};
use crate::{
    players::Reservation,
    protocol::{
        Clientbound,
        packets::{play::HeldItemSlot, play_blocks::*, play_common::IgnoredInventoryClick},
    },
    world::{self, BlockPos, BlockWorld, Snapshot},
};
use std::{collections::HashMap, io, sync::Arc};

#[derive(Debug)]
pub(super) struct CreativeSession {
    world: Arc<BlockWorld>,
    pending_chunks: HashMap<[i32; 2], Snapshot>,
    selected: u8,
}

impl CreativeSession {
    pub(super) fn new(world: Arc<BlockWorld>) -> Self {
        Self {
            world,
            pending_chunks: HashMap::new(),
            selected: 0,
        }
    }

    pub(super) fn prepare_chunks(&mut self, packets: &mut [Clientbound]) {
        for packet in packets {
            if let Clientbound::ChunkData(data) = packet {
                let snapshot = self.world.lock().snapshot(data.0);
                if let Some(edits) = &snapshot {
                    data.1 = world::edited_payload(edits);
                }
                self.pending_chunks.insert(data.0, snapshot);
            }
        }
    }

    pub(super) fn chunk_sent(&mut self, slot: &Reservation, chunk: [i32; 2]) -> io::Result<()> {
        let old = self.pending_chunks.remove(&chunk).expect("prepared chunk");
        let blocks = self.world.lock();
        let new = blocks.snapshot(chunk);
        let changes = world::changed_blocks(chunk, &old, &new);
        let updates = updates(chunk, changes, &old, &new)?;
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
    old: &Snapshot,
    new: &Snapshot,
) -> io::Result<Bodies> {
    if changes.is_empty() {
        return Ok(Arc::from([]));
    }
    let mut bodies = Vec::with_capacity(changes.len() + 1);
    for (pos, state) in changes {
        bodies.push(Clientbound::from(BlockUpdate(pos, state)).encode()?.into());
    }
    let top = world::sky_heights(new);
    if top != world::sky_heights(old) {
        bodies.push(
            Clientbound::from(LightUpdate(chunk, world::light_payload(&top)))
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
    let Some((creative, slot)) = state.creative.as_ref().zip(session.reservation.as_ref()) else {
        return Ok(reply(vec![BlockAck(sequence).into()]));
    };
    let valid = |pos: BlockPos| pos.valid() && state.chunks.tracks(pos.chunk());
    let reachable = |pos: BlockPos| valid(pos) && in_reach(state.position, pos);
    let mut blocks = creative.world.lock();
    let (positions, edit) = match action {
        Action::Dig { pos, face, start } => {
            let edit = if start && face < 6 && reachable(pos) && !matches!(blocks.get(pos), 0 | 79)
            {
                Some((pos, 0))
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
                let &(_, material) = world::HOTBAR.get(usize::from(creative.selected))?;
                (hand == 0
                    && hit.iter().all(|f| (0.0..=1.0).contains(f))
                    && reachable(support)
                    && reachable(pos)
                    && blocks.get(support) != 0
                    && blocks.get(pos) == 0
                    && !intersects_player(state.position, pos))
                .then_some((pos, material))
            });
            ([Some(support), place], edit)
        }
    };
    let ack = Clientbound::from(BlockAck(sequence)).encode()?.into();
    if state.pending_teleport.is_none()
        && let Some((pos, material)) = edit
    {
        let chunk = pos.chunk();
        let old = blocks.snapshot(chunk);
        if blocks.set(pos, material) {
            let data = updates(chunk, vec![(pos, material)], &old, &blocks.snapshot(chunk))?;
            slot.publish_blocks(chunk, data, ack);
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
    if !matches!(p.action, 0..=2) {
        return inventory(ctx).await;
    }
    interact(
        ctx,
        Action::Dig {
            pos: p.pos,
            face: p.face,
            start: p.action == 0,
        },
        p.sequence,
    )
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
    if let Some(slot) = &ctx.session.reservation
        && ctx.server.play.game_mode == crate::config::GameMode::Creative
    {
        // Acknowledge in the same FIFO as preceding edits.
        let ack = Clientbound::from(BlockAck(p.0)).encode()?.into();
        slot.correct_blocks(Vec::new(), ack);
        return Ok(Outcome::empty());
    }
    Ok(reply(vec![BlockAck(p.0).into()]))
}

pub async fn handle_slot(ctx: &mut Context<'_>, p: HeldItemSlot) -> io::Result<Outcome> {
    if let Some(state) = &mut play(ctx.session)?.creative {
        state.selected = p.slot as u8;
    }
    Ok(Outcome::empty())
}

async fn inventory(ctx: &mut Context<'_>) -> io::Result<Outcome> {
    if play(ctx.session)?.creative.is_some() {
        Ok(reply(vec![CreativeHotbar.into()]))
    } else {
        Ok(Outcome::empty())
    }
}

pub async fn handle_creative_slot(ctx: &mut Context<'_>, _: CreativeSlot) -> io::Result<Outcome> {
    inventory(ctx).await
}
pub async fn handle_inventory_click(
    ctx: &mut Context<'_>,
    _: IgnoredInventoryClick,
) -> io::Result<Outcome> {
    inventory(ctx).await
}
