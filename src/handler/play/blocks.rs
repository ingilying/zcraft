//! Thin protocol IO adapter for block & inventory packets plus chunk-write
//! reconciliation. All authoritative gameplay logic lives in `crate::engine`.
#[cfg(test)]
#[path = "blocks_tests.rs"]
mod tests;

use super::{Context, Outcome, check_gameplay_rate};
use crate::{
    engine::GameInput,
    players::Reservation,
    protocol::{
        Clientbound,
        packets::{play::HeldItemSlot, play_blocks::*},
    },
    world::{BlockPos, Chunk, World},
};
use std::{collections::HashMap, io, sync::Arc};

type PendingChunk = (Arc<Chunk>, Arc<[u8]>);

/// Tracks chunks currently in flight to a TCP connection so concurrent world
/// mutations are reconciled atomically once the chunk write completes.
#[derive(Debug)]
pub(super) struct CreativeSession {
    world: Arc<World>,
    pending_chunks: HashMap<[i32; 2], PendingChunk>,
}

impl CreativeSession {
    pub(super) fn new(world: Arc<World>) -> Self {
        Self {
            world,
            pending_chunks: HashMap::new(),
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

    /// Offload chunk loading, terrain generation, 3x3 lighting, encoding, and any
    /// dirty-cache backpressure flush onto Tokio's blocking pool so async IO
    /// worker threads never stall on CPU/disk work.
    pub(super) async fn prepare_chunks_async(
        &mut self,
        packets: &mut [Clientbound],
    ) -> io::Result<()> {
        let coords: Vec<[i32; 2]> = packets
            .iter()
            .filter_map(|packet| match packet {
                Clientbound::ChunkData(data) => Some(data.0),
                _ => None,
            })
            .collect();
        if coords.is_empty() {
            return Ok(());
        }
        let world = self.world.clone();
        let mut prepared = tokio::task::spawn_blocking(move || {
            coords
                .into_iter()
                .map(|c| (c, world.prepare_chunk(c)))
                .collect::<Vec<_>>()
        })
        .await
        .map_err(io::Error::other)?
        .into_iter();
        for packet in packets {
            if let Clientbound::ChunkData(data) = packet {
                let (c, chunk) = prepared.next().expect("prepared chunk count matches");
                debug_assert_eq!(c, data.0);
                data.1 = chunk.payload;
                self.pending_chunks.insert(c, (chunk.snapshot, chunk.light));
            }
        }
        Ok(())
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

pub async fn handle_dig(ctx: &mut Context<'_>, p: PlayerDigging) -> io::Result<Outcome> {
    let uuid = check_gameplay_rate(ctx.session)?;
    ctx.server.play.dispatch_input(
        GameInput::PlayerDig {
            uuid,
            action: p.action,
            pos: p.pos,
            face: p.face,
            sequence: p.sequence,
        },
        ctx.server.players(),
    )?;
    Ok(Outcome::empty())
}

pub async fn handle_place(ctx: &mut Context<'_>, p: BlockPlace) -> io::Result<Outcome> {
    let uuid = check_gameplay_rate(ctx.session)?;
    ctx.server.play.dispatch_input(
        GameInput::PlayerPlace {
            uuid,
            pos: p.pos,
            face: p.face,
            hand: p.hand,
            hit: p.hit,
            sequence: p.sequence,
        },
        ctx.server.players(),
    )?;
    Ok(Outcome::empty())
}

pub async fn handle_use_item(ctx: &mut Context<'_>, p: UseItem) -> io::Result<Outcome> {
    let uuid = check_gameplay_rate(ctx.session)?;
    ctx.server.play.dispatch_input(
        GameInput::PlayerUseItem {
            uuid,
            hand: p.hand,
            sequence: p.sequence,
        },
        ctx.server.players(),
    )?;
    Ok(Outcome::empty())
}

pub async fn handle_slot(ctx: &mut Context<'_>, p: HeldItemSlot) -> io::Result<Outcome> {
    let uuid = check_gameplay_rate(ctx.session)?;
    ctx.server.play.dispatch_input(
        GameInput::PlayerSelectSlot {
            uuid,
            slot: p.slot as u8,
        },
        ctx.server.players(),
    )?;
    Ok(Outcome::empty())
}

pub async fn handle_creative_slot(ctx: &mut Context<'_>, p: CreativeSlot) -> io::Result<Outcome> {
    let uuid = check_gameplay_rate(ctx.session)?;
    ctx.server.play.dispatch_input(
        GameInput::PlayerCreativeSlot {
            uuid,
            slot: p.slot,
            item: p.item.supported(),
        },
        ctx.server.players(),
    )?;
    Ok(Outcome::empty())
}

pub async fn handle_inventory_click(
    ctx: &mut Context<'_>,
    p: InventoryClickPacket,
) -> io::Result<Outcome> {
    let uuid = check_gameplay_rate(ctx.session)?;
    ctx.server.play.dispatch_input(
        GameInput::PlayerInventoryClick {
            uuid,
            window: p.window,
            state_id: p.state_id,
            click: p.click,
        },
        ctx.server.players(),
    )?;
    Ok(Outcome::empty())
}

pub async fn handle_close_window(ctx: &mut Context<'_>, p: CloseWindow) -> io::Result<Outcome> {
    let uuid = check_gameplay_rate(ctx.session)?;
    ctx.server.play.dispatch_input(
        GameInput::PlayerCloseWindow {
            uuid,
            window_id: p.window_id,
        },
        ctx.server.players(),
    )?;
    Ok(Outcome::empty())
}
