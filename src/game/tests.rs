//! Pure ECS game engine tests verifying all gameplay logic runs completely
//! isolated from network IO, sockets, and protocol framing.
use super::*;
use crate::{
    config::GameMode,
    game::{Hand, Inventory, Item, ItemStack, Player, resources::EntityIdAllocator},
    world::{BlockPos, World as VoxelWorld},
};
use std::sync::Arc;
use uuid::Uuid;

fn new_engine() -> (GameEngine, Arc<VoxelWorld>) {
    let world = Arc::new(VoxelWorld::new());
    let engine = GameEngine::new(world.clone(), Arc::new(EntityIdAllocator::default()));
    (engine, world)
}

#[test]
fn ecs_handles_movement_fall_damage_death_and_item_drop_pickup_without_network() {
    let (mut engine, world) = new_engine();
    // Preload the spawn chunk so item-drop physics ticks without force-loading.
    world.ensure_chunk_loaded([0, 0]);
    let alice_id = Uuid::from_u128(1);
    let bob_id = Uuid::from_u128(2);

    // Join Alice (Survival) with stone in slot 36.
    let mut alice = Player::at_spawn(1, alice_id, "Alice".into(), GameMode::Survival);
    alice.position = [8.5, 90.0, 8.5];
    let mut alice_inv = Inventory::default();
    let stone = Item::by_name("stone").unwrap();
    alice_inv.set_slot(36, ItemStack::new(stone, 10));

    engine.push_input(GameInput::PlayerJoin {
        player: Box::new(alice),
        inventory: Box::new(alice_inv),
    });
    engine.push_input(GameInput::PlayerTeleportConfirmed { uuid: alice_id });
    let _ = engine.tick();

    // Alice falls 25 blocks onto ground (y=64 is the flat world surface; damage = ceil(26 - 3) = 23 -> fatal).
    engine.push_input(GameInput::PlayerMove {
        uuid: alice_id,
        position: Some([8.5, 64.0, 8.5]),
        rotation: None,
        on_ground: false,
    });
    engine.push_input(GameInput::PlayerMove {
        uuid: alice_id,
        position: Some([8.5, 64.0, 8.5]),
        rotation: None,
        on_ground: true,
    });
    let outputs = engine.tick();

    assert!(
        outputs
            .iter()
            .any(|o| matches!(o, GameOutput::PlayerHurt { uuid, .. } if *uuid == alice_id))
    );
    assert!(outputs.iter().any(|o| matches!(
        o,
        GameOutput::BroadcastSystemChat(msg) if msg.contains("Alice fell from a high place")
    )));
    assert!(
        outputs
            .iter()
            .any(|o| matches!(o, GameOutput::ItemDropSpawned { .. }))
    );
    assert!(!engine.existing_drops().is_empty());

    // Join Bob at the floor surface; advance past pickup_delay (40) and walk Bob
    // to the settled drop's position so random death-scatter velocity never flakes pickup.
    let mut bob = Player::at_spawn(2, bob_id, "Bob".into(), GameMode::Survival);
    bob.position = [8.5, 64.0, 8.5];
    let mut bob_inv = Inventory::default();
    bob_inv.drain_all();
    engine.push_input(GameInput::PlayerJoin {
        player: Box::new(bob),
        inventory: Box::new(bob_inv),
    });
    engine.push_input(GameInput::PlayerTeleportConfirmed { uuid: bob_id });
    let _ = engine.tick();

    let mut collected = false;
    for step in 1..=45 {
        if step == 41
            && let Some(drop) = engine.existing_drops().first()
        {
            engine.push_input(GameInput::PlayerMove {
                uuid: bob_id,
                position: Some(drop.position),
                rotation: None,
                on_ground: true,
            });
        }
        let tick_out = engine.tick();
        if tick_out
            .iter()
            .any(|o| matches!(o, GameOutput::ItemDropCollected { collector_id: 2, .. }))
        {
            collected = true;
            break;
        }
    }
    assert!(collected, "Bob should pick up dropped items in ECS tick");
    let bob_after = engine.player_inventory(bob_id).unwrap();
    let total_stone: u32 = bob_after
        .slots()
        .iter()
        .flatten()
        .filter(|s| s.item == stone)
        .map(|s| u32::from(s.count))
        .sum();
    assert_eq!(total_stone, 10);
}

#[test]
fn ecs_ticks_food_eating_and_block_interaction_end_to_end() {
    let (mut engine, world) = new_engine();
    let uuid = Uuid::from_u128(99);

    let mut player = Player::at_spawn(10, uuid, "Steve".into(), GameMode::Survival);
    player.position = [8.5, 64.0, 8.5];
    player.food = 10;
    player.saturation = 0.0;
    let mut inv = Inventory::default();
    let steak = Item::by_name("cooked_beef").unwrap();
    inv.set_slot(36, ItemStack::new(steak, 2));
    inv.select(0);

    engine.push_input(GameInput::PlayerJoin {
        player: Box::new(player),
        inventory: Box::new(inv),
    });
    engine.push_input(GameInput::PlayerTeleportConfirmed { uuid });
    engine.push_input(GameInput::PlayerChunkTracked {
        uuid,
        chunk: [0, 0],
    });
    let _ = engine.tick();

    // Start eating steak via UseItem.
    engine.push_input(GameInput::PlayerUseItem {
        uuid,
        hand: 0,
        sequence: 1,
    });
    let _ = engine.tick();

    // Advance 32 discrete ticks -> eating completes inside tick_player_vitals_system.
    let mut outputs = Vec::new();
    for _ in 0..32 {
        outputs.extend(engine.tick());
    }
    assert!(outputs.iter().any(|o| matches!(
        o,
        GameOutput::PlayerVitalsUpdated { uuid: u, food, .. } if *u == uuid && *food == 18
    )));
    let inv_after = engine.player_inventory(uuid).unwrap();
    assert_eq!(inv_after.hand(Hand::Main).unwrap().count, 1);

    // Switch to Creative via ControlSetGameMode and break a grass block at (8, 63, 8).
    engine.push_input(GameInput::ControlSetGameMode {
        uuid,
        mode: GameMode::Creative,
    });
    engine.push_input(GameInput::PlayerDig {
        uuid,
        action: 0,
        pos: BlockPos([8, 63, 8]),
        face: 1,
        sequence: 2,
    });
    let outputs = engine.tick();
    assert!(outputs.iter().any(|o| matches!(
        o,
        GameOutput::WorldMutatedWithAck { actor, sequence: 2, .. } if *actor == uuid
    )));
    assert_eq!(world.block_at(BlockPos([8, 63, 8])), crate::game::AIR_STATE);
}

#[test]
fn entity_id_allocator_wraps_safely_and_skips_active_ids() {
    let alloc = EntityIdAllocator::new(i32::MAX - 1);
    let a = alloc.allocate().unwrap();
    let b = alloc.allocate().unwrap();
    assert_eq!(a, i32::MAX - 1);
    assert_eq!(b, i32::MAX);

    // Wraps to 1, then 2.
    let c = alloc.allocate().unwrap();
    let d = alloc.allocate().unwrap();
    assert_eq!(c, 1);
    assert_eq!(d, 2);

    // Release 1, keep 2 active; a new allocator starting at 1 with 2 active skips 2.
    alloc.release(c);
    let alloc2 = EntityIdAllocator::new(i32::MAX);
    assert_eq!(alloc2.allocate(), Some(i32::MAX));
    assert_eq!(alloc2.allocate(), Some(1));
    assert_eq!(alloc2.allocate(), Some(2));
    // Releasing 1 allows wrap-around to reuse 1 while skipping active 2.
    alloc2.release(1);
    alloc2.release(i32::MAX);
    let alloc3 = EntityIdAllocator::new(i32::MAX);
    assert_eq!(alloc3.allocate(), Some(i32::MAX));
    assert_eq!(alloc3.allocate(), Some(1));
    alloc3.release(i32::MAX);
    assert_eq!(alloc3.allocate(), Some(2));
}
