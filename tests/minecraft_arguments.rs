use zcraft::command::{arguments::*, *};

fn parse<T: Clone + Send + Sync + 'static>(kind: MinecraftArgument, text: &str) -> T {
    let units: Vec<_> = text.encode_utf16().collect();
    let mut reader = StringReader::new(&units);
    let ArgumentValue::Custom(value) = kind.parse(&mut reader).unwrap() else {
        panic!("not typed")
    };
    assert!(reader.remaining().is_empty(), "{text}");
    value.downcast_ref::<T>().unwrap().clone()
}
fn source() -> SourceStack {
    SourceStack::new(CommandSource {
        kind: SourceKind::Console,
        permission: 4,
        origin: None,
    })
    .positioned(
        Dimension {
            id: "minecraft:overworld".into(),
            coordinate_scale: 1.0,
        },
        [-0.25, 64.0, 8.5],
    )
    .unwrap()
    .with_entity(Some(ExecutingEntity {
        uuid: uuid::Uuid::nil(),
        eye_height: 1.62,
    }))
    .unwrap()
}
fn close(a: [f64; 3], b: [f64; 3]) {
    for (a, b) in a.into_iter().zip(b) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }
}
#[test]
fn typed_scalars_ranges_and_coordinate_evaluation() {
    assert_eq!(
        parse::<ResourceLocation>(MinecraftArgument::ResourceLocation, "stone").to_string(),
        "minecraft:stone"
    );
    assert_eq!(
        parse::<uuid::Uuid>(MinecraftArgument::Uuid, "1-2-3-4-5").to_string(),
        "00000001-0002-0003-0004-000000000005"
    );
    assert!(parse::<IntRange>(MinecraftArgument::IntRange, "-2..3").contains(3));
    assert!(!parse::<FloatRange>(MinecraftArgument::FloatRange, "..1.5").contains(2.0));
    assert_eq!(parse::<Axes>(MinecraftArgument::Swizzle, "zy").0, 6);
    assert_eq!(
        parse::<Anchor>(MinecraftArgument::EntityAnchor, "eyes"),
        Anchor::Eyes
    );
    let s = source();
    assert_eq!(
        parse::<BlockPos>(MinecraftArgument::BlockPos, "~ ~ ~")
            .resolve(&s)
            .unwrap(),
        [-1, 64, 8]
    );
    close(
        parse::<Vec3>(
            MinecraftArgument::Vec3 {
                center_correct: true,
            },
            "-1 64 2",
        )
        .resolve(&s)
        .unwrap(),
        [-0.5, 64.0, 2.5],
    );
    close(
        parse::<Vec3>(
            MinecraftArgument::Vec3 {
                center_correct: true,
            },
            "-1.0 64 2.0",
        )
        .resolve(&s)
        .unwrap(),
        [-1.0, 64.0, 2.0],
    );
    close(
        parse::<Vec3>(
            MinecraftArgument::Vec3 {
                center_correct: false,
            },
            "~1 ~-2 ~",
        )
        .resolve(&s)
        .unwrap(),
        [0.75, 62.0, 8.5],
    );
    let local = parse::<Vec3>(
        MinecraftArgument::Vec3 {
            center_correct: true,
        },
        "^1 ^2 ^3",
    );
    close(local.resolve(&s).unwrap(), [0.75, 66.0, 11.5]);
    close(
        local
            .resolve(&s.clone().with_rotation([90.0, 0.0]).unwrap())
            .unwrap(),
        [-3.25, 66.0, 9.5],
    );
    close(
        local.resolve(&s.clone().with_anchor(Anchor::Eyes)).unwrap(),
        [0.75, 67.62, 11.5],
    );
    let forward = parse::<Vec3>(
        MinecraftArgument::Vec3 {
            center_correct: false,
        },
        "^ ^ ^1",
    );
    close(
        forward
            .resolve(&s.clone().with_rotation([0.0, 90.0]).unwrap())
            .unwrap(),
        [-0.25, 63.0, 8.5],
    );
    assert_eq!(
        parse::<Rotation>(MinecraftArgument::Rotation, "~10 ~-20")
            .resolve(&s.clone().with_rotation([30.0, 40.0]).unwrap())
            .unwrap(),
        [40.0, 20.0]
    );
    assert_eq!(
        parse::<Angle>(MinecraftArgument::Angle, "~200")
            .resolve(&s)
            .unwrap(),
        -160.0
    );
    assert_eq!(
        parse::<ColumnPos>(MinecraftArgument::ColumnPos, "~ ~")
            .resolve(&s)
            .unwrap(),
        [-1, 8]
    );
    assert_eq!(
        parse::<Vec2>(
            MinecraftArgument::Vec2 {
                center_correct: true
            },
            "1 -2"
        )
        .resolve(&s)
        .unwrap(),
        [1.5, -1.5]
    );
    let no_position = SourceStack::new(CommandSource {
        kind: SourceKind::Console,
        permission: 4,
        origin: None,
    });
    assert!(local.resolve(&no_position).is_err());
    assert!(
        parse::<BlockPos>(MinecraftArgument::BlockPos, "~ ~ ~")
            .resolve(&no_position)
            .is_err()
    );
    close(
        parse::<Vec3>(
            MinecraftArgument::Vec3 {
                center_correct: false,
            },
            "1 2 3",
        )
        .resolve(&no_position)
        .unwrap(),
        [1.0, 2.0, 3.0],
    );
}
fn kind(s: &str) -> MinecraftArgument {
    match s {
        "resource" => MinecraftArgument::ResourceLocation,
        "uuid" => MinecraftArgument::Uuid,
        "int_range" => MinecraftArgument::IntRange,
        "float_range" => MinecraftArgument::FloatRange,
        "angle" => MinecraftArgument::Angle,
        "rotation" => MinecraftArgument::Rotation,
        "swizzle" => MinecraftArgument::Swizzle,
        "anchor" => MinecraftArgument::EntityAnchor,
        "block_pos" => MinecraftArgument::BlockPos,
        "column_pos" => MinecraftArgument::ColumnPos,
        "vec3" => MinecraftArgument::Vec3 {
            center_correct: true,
        },
        "vec2" => MinecraftArgument::Vec2 {
            center_correct: true,
        },
        _ => panic!("unknown kind"),
    }
}
#[test]
fn official_java_1_21_1_syntax_and_cursor_fixture() {
    for line in include_str!("fixtures/minecraft-arguments-1.21.1.tsv")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let f: Vec<_> = line.split('\t').collect();
        let bytes = if f[1] == "-" {
            Vec::new()
        } else {
            (0..f[1].len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&f[1][i..i + 2], 16).unwrap())
                .collect()
        };
        let input = String::from_utf8(bytes).unwrap();
        let units: Vec<_> = input.encode_utf16().collect();
        let mut reader = StringReader::new(&units);
        let result = kind(f[0]).parse(&mut reader);
        assert_eq!(
            result.is_ok(),
            f[3] == "ok",
            "{} {input:?}: {result:?}",
            f[0]
        );
        assert_eq!(
            reader.cursor(),
            f[2].parse::<usize>().unwrap(),
            "{} {input:?}",
            f[0]
        );
    }
}
#[test]
#[ignore = "requires Java and a local official 1.21.1 server bundler"]
fn regenerate_against_official_minecraft_argument_parsers() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let jar = std::env::var_os("ZCRAFT_TEST_SERVER_JAR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("target/zcraft-data/1.21.1/server.jar"));
    let output = std::process::Command::new("python3")
        .arg(root.join("tools/check-minecraft-arguments.py"))
        .arg(jar)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("fixtures/minecraft-arguments-1.21.1.tsv")
    );
}
