use std::collections::BTreeMap;
use uuid::Uuid;
use zcraft::{
    command::{CommandSource, SourceKind},
    config::{Config, GameMode},
    context::ServerContext,
    game::{Inventory, ItemStack, Player, item_by_name, registry},
    world::{BlockPos, FlatGeneratorConfig, FlatLayerConfig, World},
};

#[tokio::test]
async fn anvil_superflat_world_survives_restart_via_save_all() {
    let dir = tempfile::tempdir().unwrap();
    let world_dir = dir.path().join("world");
    let custom_flat = FlatGeneratorConfig {
        base_y: -64,
        biome: "minecraft:plains".into(),
        layers: vec![
            FlatLayerConfig {
                block: "minecraft:bedrock".into(),
                height: 1,
            },
            FlatLayerConfig {
                block: "minecraft:stone".into(),
                height: 3,
            },
            FlatLayerConfig {
                block: "minecraft:grass_block".into(),
                height: 1,
            },
        ],
    };

    let config = Config {
        world_dir: Some(world_dir.to_string_lossy().into_owned()),
        flat: Some(custom_flat.clone()),
        game_mode: GameMode::Creative,
        ..Config::default()
    };
    let server = ServerContext::from_config(&config).await.unwrap();
    let world = server.play.world();
    assert_eq!(
        world.block_at(BlockPos([0, -64, 0])),
        zcraft::game::BEDROCK_STATE
    );
    assert_eq!(
        world.block_at(BlockPos([0, -63, 0])),
        zcraft::game::STONE_STATE
    );
    assert_eq!(
        world.block_at(BlockPos([0, -60, 0])),
        zcraft::game::GRASS_BLOCK_STATE
    );
    assert_eq!(
        world.block_at(BlockPos([0, -59, 0])),
        zcraft::game::AIR_STATE
    );
    assert_eq!(world.spawn_position(), [8.5, -58.0, 8.5]);

    // Mutate blocks across two different regions (r.0.0.mca and r.-1.-1.mca).
    let mut props = BTreeMap::new();
    props.insert("facing".into(), "west".into());
    let stairs = registry()
        .resolve_state("minecraft:oak_stairs", &props)
        .unwrap();
    let pos_a = BlockPos([4, -59, 6]);
    let pos_b = BlockPos([-20, -58, -10]);
    let pos_break = BlockPos([2, -60, 2]);
    assert!(world.set_block(pos_a, stairs).is_some());
    assert!(world.set_block(pos_b, zcraft::game::GLASS_STATE).is_some());
    assert!(
        world
            .set_block(pos_break, zcraft::game::AIR_STATE)
            .is_some()
    );

    // Change time and weather.
    server.play.set_day_time(14_500);
    server.play.set_weather(true, true, 4_200);

    // Trigger /save-all as console OP.
    let console = CommandSource {
        kind: SourceKind::Console,
        permission: 4,
        origin: None,
    };
    let result = server.execute_command(&console, "save-all").unwrap();
    assert!(result.success);
    assert!(result.feedback[0].contains("2 chunk(s)"));
    assert!(world_dir.join("level.dat").exists());
    assert!(world_dir.join("region/r.0.0.mca").exists());
    assert!(world_dir.join("region/r.-1.-1.mca").exists());

    // Save a player profile into playerdata/<uuid>.dat.
    let uuid = Uuid::from_u128(42);
    let mut player = Player::at_spawn(1, uuid, "Alex".into(), GameMode::Creative);
    player.position = [100.5, -58.0, -40.25];
    player.yaw = 135.0;
    player.pitch = 10.0;
    player.on_ground = true;
    let mut inv = Inventory::default();
    inv.select(5);
    let shovel = item_by_name("iron_shovel").unwrap().id;
    inv.set_creative(36, ItemStack::with_damage(shovel, 1, 9));
    server.play.save_player(&player, &inv).unwrap();
    assert!(world_dir.join(format!("playerdata/{uuid}.dat")).exists());

    drop(server);

    // Reopen world WITHOUT passing `flat` in config; level.dat must restore everything!
    let reopened = World::open(&world_dir, None).unwrap();
    assert_eq!(reopened.flat_config(), Some(&custom_flat));
    assert_eq!(reopened.block_at(pos_a), stairs);
    assert_eq!(reopened.block_at(pos_b), zcraft::game::GLASS_STATE);
    assert_eq!(reopened.block_at(pos_break), zcraft::game::AIR_STATE);
    assert_eq!(
        reopened.block_at(BlockPos([1, -60, 1])),
        zcraft::game::GRASS_BLOCK_STATE
    );
    assert_eq!(reopened.time().day_time, 14_500);
    assert!(reopened.weather().raining);
    assert!(reopened.weather().thundering);

    let restored = reopened.load_player(uuid).unwrap().unwrap();
    assert_eq!(restored.position, [100.5, -58.0, -40.25]);
    assert_eq!(restored.yaw, 135.0);
    assert_eq!(restored.pitch, 10.0);
    assert!(restored.on_ground);
    assert_eq!(restored.game_mode, GameMode::Creative);
    assert_eq!(restored.inventory.selected(), 5);
    assert_eq!(
        restored.inventory.slots()[36],
        ItemStack::with_damage(shovel, 1, 9)
    );
}

#[test]
fn save_failure_preserves_dirty_chunks_for_retry() {
    let dir = tempfile::tempdir().unwrap();
    let world_dir = dir.path().join("world");
    let world = World::open(&world_dir, None).unwrap();

    let pos = BlockPos([5, 64, 5]);
    assert!(world.set_block(pos, zcraft::game::GLASS_STATE).is_some());

    // Sabotage level.dat by replacing it with a directory so save_level fails.
    let level_path = world_dir.join("level.dat");
    std::fs::remove_file(&level_path).unwrap();
    std::fs::create_dir(&level_path).unwrap();

    assert!(
        world.save_all().is_err(),
        "save_level failure must propagate"
    );

    // Repair the directory obstacle and retry save_all: dirty chunk must still be flushed!
    std::fs::remove_dir(&level_path).unwrap();
    let summary = world.save_all().unwrap();
    assert!(summary.persisted);
    assert_eq!(summary.chunks_saved, 1);

    let reopened = World::open(&world_dir, None).unwrap();
    assert_eq!(reopened.block_at(pos), zcraft::game::GLASS_STATE);
}

#[test]
fn concurrent_saves_and_edits_never_lose_or_regress_data() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let world_dir = dir.path().join("world");
    let world = Arc::new(World::open(&world_dir, None).unwrap());

    let mut handles = Vec::new();
    for worker in 0..4i32 {
        let w = world.clone();
        handles.push(std::thread::spawn(move || {
            for step in 0..16i32 {
                let pos = BlockPos([worker * 16 + step, 64, worker]);
                assert!(w.set_block(pos, zcraft::game::STONE_STATE).is_some());
                w.set_day_time(i64::from(worker * 100 + step));
                if step % 4 == 0 {
                    w.save_all().unwrap();
                }
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    world.save_all().unwrap();

    let reopened = World::open(&world_dir, None).unwrap();
    for worker in 0..4i32 {
        for step in 0..16i32 {
            let pos = BlockPos([worker * 16 + step, 64, worker]);
            assert_eq!(reopened.block_at(pos), zcraft::game::STONE_STATE);
        }
    }
}

#[tokio::test]
async fn corrupt_playerdata_rejects_login_without_overwriting_file() {
    let dir = tempfile::tempdir().unwrap();
    let world_dir = dir.path().join("world");
    let config = Config {
        world_dir: Some(world_dir.to_string_lossy().into_owned()),
        ..Config::default()
    };
    let server = ServerContext::from_config(&config).await.unwrap();

    let uuid = Uuid::from_u128(0xdead_beef);
    let player_dir = world_dir.join("playerdata");
    std::fs::create_dir_all(&player_dir).unwrap();
    let player_file = player_dir.join(format!("{uuid}.dat"));
    let corrupt_bytes = b"corrupt-nbt-must-not-be-overwritten";
    std::fs::write(&player_file, corrupt_bytes).unwrap();

    let mut session = zcraft::handler::Session::default();
    session.profile = Some(zcraft::identity::Profile {
        uuid,
        name: "CorruptUser".into(),
        properties: Vec::new(),
    });
    let mut ctx = zcraft::context::Context {
        session: &mut session,
        server: &server,
    };
    let err = zcraft::handler::play::begin(&mut ctx)
        .err()
        .expect("corrupt playerdata must reject Play entry");
    assert!(
        session.play.is_none(),
        "session must not enter Play on error: {err}"
    );
    assert_eq!(
        std::fs::read(&player_file).unwrap(),
        corrupt_bytes,
        "corrupt playerdata file must remain untouched"
    );
}

#[test]
fn custom_biome_is_validated_and_encoded_in_flat_generator() {
    let invalid = FlatGeneratorConfig {
        biome: "minecraft:not_a_real_biome".into(),
        ..FlatGeneratorConfig::default()
    };
    assert!(invalid.validate().is_err());

    let desert_id = zcraft::registry_data::biome_id("minecraft:desert").unwrap();
    assert_ne!(desert_id, 0);
    let desert_cfg = FlatGeneratorConfig {
        biome: "minecraft:desert".into(),
        ..FlatGeneratorConfig::default()
    };
    let generator = zcraft::world::FlatGenerator::from_config(desert_cfg).unwrap();
    assert_eq!(generator.biome_id(), desert_id);
    assert_ne!(
        generator.terrain_payload().as_ref(),
        zcraft::world::FlatGenerator::default()
            .terrain_payload()
            .as_ref()
    );
}

#[test]
fn pluggable_chunk_generator_produces_distinct_per_chunk_terrain() {
    use std::sync::Arc;
    use zcraft::world::{Chunk, ChunkGenerator};

    #[derive(Debug)]
    struct CheckerboardGenerator;

    impl ChunkGenerator for CheckerboardGenerator {
        fn generate_chunk(&self, cx: i32, cz: i32) -> Arc<Chunk> {
            let biome = if (cx + cz).rem_euclid(2) == 0 {
                zcraft::registry_data::biome_id("minecraft:plains").unwrap()
            } else {
                zcraft::registry_data::biome_id("minecraft:desert").unwrap()
            };
            let mut chunk = Chunk::empty(biome);
            let top_block = if (cx + cz).rem_euclid(2) == 0 {
                zcraft::game::GRASS_BLOCK_STATE
            } else {
                zcraft::game::STONE_STATE
            };
            chunk.set_block(0, 64, 0, top_block);
            Arc::new(chunk)
        }

        fn spawn_position(&self) -> [f64; 3] {
            [0.5, 66.0, 0.5]
        }
    }

    let world = World::with_generator(Arc::new(CheckerboardGenerator));
    assert_eq!(
        world.block_at(BlockPos([0, 64, 0])),
        zcraft::game::GRASS_BLOCK_STATE
    );
    assert_eq!(
        world.block_at(BlockPos([16, 64, 0])),
        zcraft::game::STONE_STATE
    );
    assert!(
        world
            .set_block(BlockPos([16, 65, 0]), zcraft::game::GLASS_STATE)
            .is_some()
    );
    assert_eq!(
        world.block_at(BlockPos([16, 65, 0])),
        zcraft::game::GLASS_STATE
    );
}

#[test]
fn chest_block_entity_persists_in_anvil_and_clears_when_broken() {
    let dir = tempfile::tempdir().unwrap();
    let world_dir = dir.path().join("world");
    let world = World::open(&world_dir, None).unwrap();

    let chest_pos = BlockPos([4, 64, 4]);
    let chest_state = zcraft::game::block_by_name("chest").unwrap().default_state;
    assert!(world.set_block(chest_pos, chest_state).is_some());

    let diamond = item_by_name("diamond").unwrap().id;
    let pickaxe = item_by_name("iron_pickaxe").unwrap().id;
    let stack_a = ItemStack::new(diamond, 16).unwrap();
    let stack_b = ItemStack::with_damage(pickaxe, 1, 25).unwrap();

    let updated = world
        .update_chest(chest_pos, |slots| {
            slots[0] = Some(stack_a);
            slots[26] = Some(stack_b);
            true
        })
        .unwrap();
    assert_eq!(updated[0], Some(stack_a));
    assert_eq!(updated[26], Some(stack_b));

    let summary = world.save_all().unwrap();
    assert_eq!(summary.chunks_saved, 1);

    // Reopen from Anvil and verify chest BlockEntity contents survived.
    let reopened = World::open(&world_dir, None).unwrap();
    let items = reopened.chest_items(chest_pos).unwrap();
    assert_eq!(items[0], Some(stack_a));
    assert_eq!(items[26], Some(stack_b));

    // Breaking the chest removes its BlockEntity.
    assert!(
        reopened
            .set_block(chest_pos, zcraft::game::AIR_STATE)
            .is_some()
    );
    assert!(reopened.chest_items(chest_pos).is_none());
}

#[tokio::test]
async fn overworld_seed_and_edits_persist_via_anvil_level_dat() {
    let dir = tempfile::tempdir().unwrap();
    let world_dir = dir.path().join("overworld");
    let config = Config {
        world_dir: Some(world_dir.to_string_lossy().into_owned()),
        world_type: zcraft::config::WorldType::Overworld,
        seed: Some(2026),
        ..Config::default()
    };
    let server = ServerContext::from_config(&config).await.unwrap();
    let world = server.play.world();
    assert_eq!(world.overworld_seed(), Some(2026));
    assert_eq!(
        world.block_at(BlockPos([0, -64, 0])),
        zcraft::game::BEDROCK_STATE
    );
    assert!(world.spawn_position()[1] >= f64::from(zcraft::world::SEA_LEVEL));

    let sample_pos = BlockPos([3, 120, 7]);
    let edit_pos = BlockPos([5, 100, 5]);
    let natural_state = world.block_at(sample_pos);
    assert!(
        world
            .set_block(edit_pos, zcraft::game::GLASS_STATE)
            .is_some()
    );
    world.save_all().unwrap();
    drop(server);

    // Reopen WITHOUT passing world_type or seed: level.dat must restore Overworld(2026).
    let reopened = World::open(&world_dir, None).unwrap();
    assert_eq!(reopened.overworld_seed(), Some(2026));
    assert_eq!(reopened.block_at(sample_pos), natural_state);
    assert_eq!(reopened.block_at(edit_pos), zcraft::game::GLASS_STATE);
}
