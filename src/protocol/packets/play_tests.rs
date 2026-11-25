use super::*;
use crate::identity::ProfileProperty;

fn encoded(packet: &impl Encode) -> Vec<u8> {
    let mut out = Vec::new();
    packet.encode(&mut out).unwrap();
    out
}

fn f32_field(reader: &mut Reader<'_>) -> f32 {
    f32::from_be_bytes(reader.take(4).unwrap().try_into().unwrap())
}

fn f64_field(reader: &mut Reader<'_>) -> f64 {
    f64::from_be_bytes(reader.take(8).unwrap().try_into().unwrap())
}

#[test]
fn join_game_has_exact_767_field_order() {
    let bytes = encoded(&JoinGame {
        entity_id: 42,
        view_distance: 2,
        game_mode: GameMode::Spectator,
    });
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.take(4).unwrap(), 42i32.to_be_bytes());
    assert_eq!(reader.take(1).unwrap(), [0]);
    assert_eq!(reader.varint().unwrap(), 1);
    assert_eq!(reader.string(32767).unwrap(), "minecraft:overworld");
    assert_eq!(reader.varint().unwrap(), 20);
    assert_eq!(reader.varint().unwrap(), 2);
    assert_eq!(reader.varint().unwrap(), 2);
    assert_eq!(reader.take(3).unwrap(), [0, 1, 0]);
    assert_eq!(reader.varint().unwrap(), 0);
    assert_eq!(reader.string(32767).unwrap(), "minecraft:overworld");
    assert_eq!(reader.i64().unwrap(), 0);
    assert_eq!(reader.take(5).unwrap(), [3, 255, 0, 1, 0]);
    assert_eq!(reader.varint().unwrap(), 0);
    assert_eq!(reader.take(1).unwrap(), [0]);
    reader.finish().unwrap();
}

#[test]
fn player_info_preserves_trusted_identity_and_signed_properties() {
    let profile = Profile {
        uuid: Uuid::from_bytes([7; 16]),
        name: "Steve".into(),
        properties: vec![
            ProfileProperty {
                name: "textures".into(),
                value: "value".into(),
                signature: Some("signature".into()),
            },
            ProfileProperty {
                name: "other".into(),
                value: "data".into(),
                signature: None,
            },
        ],
    };
    let bytes = encoded(&PlayerInfo {
        game_mode: GameMode::Spectator,
        profile: profile.clone(),
    });
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.take(1).unwrap(), [0x0d]);
    assert_eq!(reader.varint().unwrap(), 1);
    assert_eq!(reader.take(16).unwrap(), profile.uuid.as_bytes());
    assert_eq!(reader.string(16).unwrap(), profile.name);
    assert_eq!(reader.varint().unwrap(), 2);
    for property in profile.properties {
        assert_eq!(reader.string(32767).unwrap(), property.name);
        assert_eq!(reader.string(32767).unwrap(), property.value);
        assert_eq!(boolean(&mut reader).unwrap(), property.signature.is_some());
        if let Some(signature) = property.signature {
            assert_eq!(reader.string(32767).unwrap(), signature);
        }
    }
    assert_eq!(reader.varint().unwrap(), 3);
    assert!(boolean(&mut reader).unwrap());
    reader.finish().unwrap();
}

#[test]
fn position_and_spawn_are_absolute_and_correctly_packed() {
    let bytes = encoded(&PlayerPosition::spawn(123));
    let mut reader = Reader::new(&bytes);
    assert_eq!(f64_field(&mut reader), SPAWN_X);
    assert_eq!(f64_field(&mut reader), SPAWN_Y);
    assert_eq!(f64_field(&mut reader), SPAWN_Z);
    assert_eq!(f32_field(&mut reader), 0.0);
    assert_eq!(f32_field(&mut reader), 0.0);
    assert_eq!(reader.take(1).unwrap(), [0]);
    assert_eq!(reader.varint().unwrap(), 123);
    reader.finish().unwrap();
    let bytes = encoded(&DefaultSpawnPosition);
    let mut reader = Reader::new(&bytes);
    let packed = reader.i64().unwrap() as u64;
    assert_eq!(packed >> 38, 8);
    assert_eq!((packed >> 12) & 0x3ff_ffff, 8);
    assert_eq!(packed & 0xfff, 64);
    assert_eq!(f32_field(&mut reader), 0.0);
    reader.finish().unwrap();
}

#[test]
fn fixed_world_packets_are_wire_correct() {
    assert!(encoded(&ChunkBatchStart).is_empty());
    assert_eq!(encoded(&ChunkBatchFinished { count: 25 }), [25]);
    assert_eq!(encoded(&CenterChunk([-1, 2])), [255, 255, 255, 255, 15, 2]);
    assert_eq!(encoded(&ChunkViewDistance(16)), [16]);
    assert_eq!(
        encoded(&ForgetChunk([-1, 2])),
        [0, 0, 0, 2, 255, 255, 255, 255]
    );
    assert_eq!(
        encoded(&ChunkData([-1, 2], Arc::from([1, 2, 3]))),
        [255, 255, 255, 255, 0, 0, 0, 2, 1, 2, 3]
    );
    assert_eq!(encoded(&KeepAlive { id: -42 }), (-42i64).to_be_bytes());
    let bytes = encoded(&PlayerAbilities(GameMode::Spectator));
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.take(1).unwrap(), [7]);
    assert_eq!(f32_field(&mut reader), 0.05);
    assert_eq!(f32_field(&mut reader), 0.1);
    reader.finish().unwrap();
    assert_eq!(
        encoded(&GameEvent::level_chunks_load_start()),
        [13, 0, 0, 0, 0]
    );
    assert_eq!(encoded(&GameEvent::start_raining()), [1, 0, 0, 0, 0]);
    assert_eq!(encoded(&GameEvent::stop_raining()), [2, 0, 0, 0, 0]);
    assert_eq!(
        encoded(&GameEvent::rain_level(0.5)),
        [7, 0x3f, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        encoded(&GameEvent::thunder_level(1.0)),
        [8, 0x3f, 0x80, 0x00, 0x00]
    );
    let bytes = encoded(&TimeUpdate {
        world_age: 40,
        time_of_day: 6040,
    });
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.i64().unwrap(), 40);
    assert_eq!(reader.i64().unwrap(), 6040);
    reader.finish().unwrap();
    let bytes = encoded(&WorldBorder);
    let mut reader = Reader::new(&bytes);
    for expected in [0.0, 0.0, 59_999_968.0, 59_999_968.0] {
        assert_eq!(f64_field(&mut reader), expected);
    }
    assert_eq!(reader.varint().unwrap(), 0);
    assert_eq!(reader.varint().unwrap(), 29_999_984);
    assert_eq!(reader.varint().unwrap(), 5);
    assert_eq!(reader.varint().unwrap(), 15);
    reader.finish().unwrap();
}

#[test]
fn disconnect_is_anonymous_nbt_and_rejects_modified_utf8_edge_cases() {
    assert_eq!(
        encoded(&PlayDisconnect {
            reason: "Bye".into()
        }),
        [8, 0, 3, b'B', b'y', b'e']
    );
    for reason in ["世界".into(), "nul\0".into(), "x".repeat(65536)] {
        assert!(PlayDisconnect { reason }.encode(&mut Vec::new()).is_err());
    }
}

fn position_bytes(x: f64, y: f64, z: f64, on_ground: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    for coordinate in [x, y, z] {
        bytes.extend_from_slice(&coordinate.to_be_bytes());
    }
    bytes.push(on_ground);
    bytes
}

#[test]
fn movement_decodes_valid_fields_and_rejects_truncation_nonfinite_and_bad_bool() {
    let bytes = position_bytes(8.5, 65.0, -8.5, 1);
    assert_eq!(
        Position::decode(&mut Reader::new(&bytes)).unwrap(),
        Position {
            x: 8.5,
            y: 65.0,
            z: -8.5,
            on_ground: true
        }
    );
    for end in 0..bytes.len() {
        assert!(Position::decode(&mut Reader::new(&bytes[..end])).is_err());
    }
    for (x, y, z, ground) in [
        (f64::NAN, 0.0, 0.0, 0),
        (0.0, f64::INFINITY, 0.0, 0),
        (0.0, 0.0, f64::NEG_INFINITY, 0),
        (30_000_001.0, 0.0, 0.0, 0),
        (0.0, 20_000_001.0, 0.0, 0),
        (0.0, 0.0, 0.0, 2),
    ] {
        assert!(Position::decode(&mut Reader::new(&position_bytes(x, y, z, ground))).is_err());
    }
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut bytes = bad.to_be_bytes().to_vec();
        bytes.extend_from_slice(&0f32.to_be_bytes());
        bytes.push(0);
        assert!(Look::decode(&mut Reader::new(&bytes)).is_err());
    }
    let mut bytes = position_bytes(1.0, 2.0, 3.0, 0);
    bytes.pop();
    bytes.extend_from_slice(&45f32.to_be_bytes());
    bytes.extend_from_slice(&(-30f32).to_be_bytes());
    bytes.push(0);
    assert_eq!(
        PositionLook::decode(&mut Reader::new(&bytes)).unwrap(),
        PositionLook {
            x: 1.0,
            y: 2.0,
            z: 3.0,
            yaw: 45.0,
            pitch: -30.0,
            on_ground: false
        }
    );
    assert!(Flying::decode(&mut Reader::new(&[2])).is_err());
}

#[test]
fn bounded_controls_and_counts_reject_invalid_values() {
    for rate in [0.0f32, -1.0, f32::INFINITY, f32::NAN] {
        assert!(ChunkBatchReceived::decode(&mut Reader::new(&rate.to_be_bytes())).is_err());
    }
    assert_eq!(
        ChunkBatchReceived::decode(&mut Reader::new(&10f32.to_be_bytes()))
            .unwrap()
            .chunks_per_tick,
        10.0
    );
    assert!(Abilities::decode(&mut Reader::new(&[7])).is_err());
    assert_eq!(Abilities::decode(&mut Reader::new(&[2])).unwrap().flags, 2);
    assert!(EntityAction::decode(&mut Reader::new(&[1, 9, 0])).is_err());
    assert!(EntityAction::decode(&mut Reader::new(&[1, 5, 101])).is_err());
    assert_eq!(
        EntityAction::decode(&mut Reader::new(&[1, 3, 0]))
            .unwrap()
            .action,
        3
    );
    assert!(HeldItemSlot::decode(&mut Reader::new(&(-1i16).to_be_bytes())).is_err());
    assert!(HeldItemSlot::decode(&mut Reader::new(&9i16.to_be_bytes())).is_err());
    assert_eq!(
        HeldItemSlot::decode(&mut Reader::new(&8i16.to_be_bytes()))
            .unwrap()
            .slot,
        8
    );
    assert!(ArmAnimation::decode(&mut Reader::new(&[2])).is_err());
    assert_eq!(
        ArmAnimation::decode(&mut Reader::new(&[1])).unwrap().hand,
        1
    );
    let mut negative = Vec::new();
    put_varint(-1, &mut negative);
    assert!(TeleportConfirm::decode(&mut Reader::new(&negative)).is_err());
    assert!(PlayerPosition::spawn(-1).encode(&mut Vec::new()).is_err());
    assert!(
        ChunkBatchFinished { count: -1 }
            .encode(&mut Vec::new())
            .is_err()
    );
}

#[test]
fn chat_session_has_uuid_expiry_and_bounded_key_arrays() {
    let mut bytes = vec![7; 16];
    bytes.extend_from_slice(&123i64.to_be_bytes());
    put_varint(3, &mut bytes);
    bytes.extend_from_slice(&[1, 2, 3]);
    put_varint(2, &mut bytes);
    bytes.extend_from_slice(&[4, 5]);
    let mut reader = Reader::new(&bytes);
    let packet = ChatSessionUpdate::decode(&mut reader).unwrap();
    reader.finish().unwrap();
    assert_eq!(packet.session_uuid, Uuid::from_bytes([7; 16]));
    assert_eq!(packet.expires_at, 123);
    assert_eq!(packet.public_key, [1, 2, 3]);
    assert_eq!(packet.signature, [4, 5]);
    for end in 0..bytes.len() {
        assert!(ChatSessionUpdate::decode(&mut Reader::new(&bytes[..end])).is_err());
    }
    let mut oversized = vec![0; 24];
    put_varint(513, &mut oversized);
    assert!(ChatSessionUpdate::decode(&mut Reader::new(&oversized)).is_err());
    let mut empty = vec![0; 24];
    empty.extend_from_slice(&[0, 1, 1]);
    assert!(ChatSessionUpdate::decode(&mut Reader::new(&empty)).is_err());
}

#[test]
fn outer_decoder_remains_responsible_for_trailing_fields() {
    let mut reader = Reader::new(&[1, 99]);
    assert_eq!(TeleportConfirm::decode(&mut reader).unwrap().id, 1);
    assert!(reader.finish().is_err());
    let bytes = (-42i64).to_be_bytes();
    let mut reader = Reader::new(&bytes);
    assert_eq!(KeepAliveResponse::decode(&mut reader).unwrap().id, -42);
    reader.finish().unwrap();
}
