use super::*;
use crate::{players::Players, protocol::packets::play::ChunkData, world};
use uuid::Uuid;

#[tokio::test]
async fn mutation_during_chunk_write_is_reconciled_before_later_updates() {
    let world = Arc::new(World::new());
    let mut creative = CreativeSession::new(world.clone());
    let players = Players::new(1);
    let slot = players.reserve(Uuid::nil()).unwrap();
    let chunk = [-1, 0];
    let pos = BlockPos([-1, 64, 0]);
    let mut batch = [Clientbound::from(ChunkData(chunk, world::chunk_payload()))];
    creative.prepare_chunks(&mut batch);
    assert!(world.set_block(pos, 1).is_some());
    creative.chunk_sent(&slot, chunk).unwrap();
    {
        let mutation = world.set_block(pos, 0).unwrap();
        let center_light = mutation
            .light_changes
            .into_iter()
            .find(|(c, _)| *c == chunk)
            .map(|(_, l)| l);
        let bodies = encode_chunk_updates(chunk, mutation.block_changes, center_light).unwrap();
        slot.publish_blocks(
            chunk,
            bodies,
            Clientbound::from(BlockAck(7)).encode().unwrap().into(),
        );
    }
    let inbox = slot.inbox();
    let mut output = Vec::new();
    for _ in 0..2 {
        let delivery = tokio::time::timeout(std::time::Duration::from_secs(1), inbox.next())
            .await
            .unwrap();
        output.extend(delivery.bodies().unwrap());
    }
    let body = |state| Clientbound::from(BlockUpdate(pos, state)).encode().unwrap();
    assert_eq!(&*output[0], body(1));
    assert_eq!(&*output[2], body(0));
    assert_eq!(&*output[4], &[0x05, 7]);
}
