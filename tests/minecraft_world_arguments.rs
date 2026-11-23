use rand::{SeedableRng, rngs::StdRng};
use std::collections::HashSet;
use uuid::Uuid;
use zcraft::command::{arguments::*, *};

fn typed<T: Clone + Send + Sync + 'static>(parser: &dyn ArgumentParser, input: &str) -> T {
    let units: Vec<_> = input.encode_utf16().collect();
    let mut r = StringReader::new(&units);
    let ArgumentValue::Custom(v) = parser.parse(&mut r).unwrap() else {
        panic!()
    };
    assert!(r.remaining().is_empty(), "{input}");
    v.downcast_ref::<T>().unwrap().clone()
}
fn rejects(parser: &dyn ArgumentParser, input: &str) {
    let u: Vec<_> = input.encode_utf16().collect();
    assert!(parser.parse(&mut StringReader::new(&u)).is_err(), "{input}");
}
fn source(permission: u8) -> SourceStack {
    SourceStack::new(CommandSource {
        kind: SourceKind::Player(Uuid::from_u128(1)),
        permission,
        origin: None,
    })
    .positioned(
        Dimension {
            id: "minecraft:overworld".into(),
            coordinate_scale: 1.0,
        },
        [0.0, 64.0, 0.0],
    )
    .unwrap()
    .with_entity(Some(ExecutingEntity {
        uuid: Uuid::from_u128(2),
        eye_height: 1.62,
    }))
    .unwrap()
}
fn entities() -> Vec<EntitySnapshot> {
    [
        (
            1,
            "Alice",
            0.0,
            "minecraft:overworld",
            EntityGameMode::Creative,
        ),
        (
            2,
            "Bob",
            4.0,
            "minecraft:overworld",
            EntityGameMode::Spectator,
        ),
        (
            3,
            "Cara",
            20.0,
            "minecraft:overworld",
            EntityGameMode::Creative,
        ),
        (
            4,
            "Dora",
            1.0,
            "minecraft:the_nether",
            EntityGameMode::Creative,
        ),
    ]
    .map(|(id, name, x, world, mode)| EntitySnapshot {
        uuid: Uuid::from_u128(id),
        name: name.into(),
        entity_type: ResourceLocation::parse("player").unwrap(),
        world: world.into(),
        position: [x, 64.0, 0.0],
        rotation: [if id == 2 { 175.0 } else { 0.0 }, 0.0],
        width: 0.6,
        height: 1.8,
        game_mode: Some(mode),
    })
    .to_vec()
}
fn names(
    input: &str,
    source: &SourceStack,
    entities: &[EntitySnapshot],
) -> Result<Vec<String>, EntityQueryError> {
    let q: EntityQuery = typed(&EntityArgument::new(false, false), input);
    q.resolve(source, entities, &mut StdRng::seed_from_u64(7))
        .map(|entities| entities.iter().map(|e| e.name.clone()).collect())
}
#[test]
fn selectors_use_executor_permissions_dimensions_and_real_filters() {
    let entities = entities();
    let s = source(2);
    assert_eq!(names("@s", &s, &entities).unwrap(), ["Bob"]); // Not authenticated Alice.
    assert_eq!(names("@p", &s, &entities).unwrap(), ["Alice"]);
    assert_eq!(
        names("@a[distance=..5]", &s, &entities).unwrap(),
        ["Alice", "Bob"]
    );
    assert_eq!(
        names("@a[distance=..30,sort=furthest,limit=1]", &s, &entities).unwrap(),
        ["Cara"]
    );
    assert_eq!(
        names(
            "@a[gamemode=!spectator,name=!Alice,distance=..30]",
            &s,
            &entities
        )
        .unwrap(),
        ["Cara"]
    );
    assert_eq!(
        names("@e[type=player,x=3,y=64,z=0,dx=2,dy=2,dz=1]", &s, &entities).unwrap(),
        ["Bob"]
    );
    assert_eq!(
        names("@e[type=!player]", &s, &entities).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        names("@a[y_rotation=170..-170]", &s, &entities).unwrap(),
        ["Bob"]
    );
    assert_eq!(names("Alice", &source(0), &entities).unwrap(), ["Alice"]);
    assert_eq!(
        names(&Uuid::from_u128(2).to_string(), &source(0), &entities).unwrap(),
        ["Bob"]
    );
    assert_eq!(
        names("@a", &source(0), &entities),
        Err(EntityQueryError::Permission)
    );
    let q: EntityQuery = typed(&EntityArgument::new(true, true), "Missing");
    assert_eq!(
        q.resolve(&s, &entities, &mut StdRng::seed_from_u64(1))
            .unwrap_err(),
        EntityQueryError::NoEntities
    );
    let q: EntityQuery = typed(&EntityArgument::new(true, true), "@r");
    let mut seen = HashSet::new();
    for seed in 0..32 {
        seen.insert(
            q.resolve(&s, &entities, &mut StdRng::seed_from_u64(seed))
                .unwrap()[0]
                .uuid,
        );
    }
    assert!(
        seen.len() > 1,
        "random selection must not mean first player"
    );
    assert_eq!(
        q.resolve(&s, &entities, &mut StdRng::seed_from_u64(42))
            .unwrap()[0]
            .uuid,
        q.resolve(&s, &entities, &mut StdRng::seed_from_u64(42))
            .unwrap()[0]
            .uuid
    );
}
#[test]
fn selector_parser_rejects_unsupported_and_incompatible_semantics() {
    let multiple = EntityArgument::new(false, false);
    for input in [
        "@z",
        "@a[limit=0]",
        "@s[limit=1]",
        "@a[limit=+1]",
        "@a[x=1e2]",
        "@a[distance=-1..5]",
        "@a[x=NaN]",
        "@a[x=1,x=2]",
        "@a[name=Alice,name=Bob]",
        "@a[name=!Alice,name=Bob]",
        "@a[sort=unknown]",
        "@a[scores={x=1}]",
        "@a[tag=foo]",
        "@a[team=red]",
        "@e[type=zombie]",
        "@a[nbt={}]",
        "@a[predicate=x:y]",
        "@a[advancements={}]",
        "@a[distance=..]",
    ] {
        rejects(&multiple, input);
    }
    rejects(&EntityArgument::new(true, false), "@a");
    rejects(&EntityArgument::new(false, true), "@e");
    let _: EntityQuery = typed(&EntityArgument::new(true, true), "@e[type=player,limit=1]");
    let mut snapshot = entities();
    snapshot[0].position[0] = f64::NAN;
    assert_eq!(
        names("@a", &source(2), &snapshot),
        Err(EntityQueryError::InvalidSnapshot)
    );
}
#[test]
fn block_and_item_syntax_resolve_only_existing_capabilities() {
    for (name, id) in [
        ("air", 0),
        ("stone", 1),
        ("dirt", 10),
        ("bedrock", 79),
        ("glass", 519),
        ("grass_block[snowy=false]", 9),
    ] {
        assert_eq!(
            typed::<BlockStateSpec>(&BlockArgument, name)
                .resolve_existing()
                .unwrap(),
            id
        );
    }
    assert!(
        typed::<BlockStateSpec>(&BlockArgument, "grass_block[snowy=true]")
            .resolve_existing()
            .is_err()
    );
    assert!(
        typed::<BlockStateSpec>(&BlockArgument, "stone[wrong=value]")
            .resolve_existing()
            .is_err()
    );
    assert!(
        typed::<BlockStateSpec>(&BlockArgument, "oak_log[axis=x]")
            .resolve_existing()
            .is_err()
    );
    for (name, id) in [("stone", 1), ("dirt", 28), ("glass", 188)] {
        assert_eq!(
            typed::<ItemSpec>(&ItemArgument, name)
                .resolve_existing()
                .unwrap(),
            id
        );
    }
    assert!(
        typed::<ItemSpec>(&ItemArgument, "diamond")
            .resolve_existing()
            .is_err()
    );
    for input in [
        "stone[x=1,x=2]",
        "stone{foo:1}",
        "#minecraft:logs",
        "stone[x=]",
        "stone[x=1",
    ] {
        rejects(&BlockArgument, input);
    }
    for input in [
        "stone[minecraft:custom_name='x']",
        "stone{}",
        "#minecraft:planks",
    ] {
        rejects(&ItemArgument, input);
    }
    assert_eq!(EntityArgument::new(true, true).wire(), Some((6, vec![3])));
    assert_eq!(BlockArgument.wire(), Some((12, vec![])));
    assert_eq!(ItemArgument.wire(), Some((14, vec![])));
}
