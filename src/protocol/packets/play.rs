//! Minimal flat-world Play packets for Minecraft Java 1.21.1 (767).
//!
//! Field layouts and packet IDs are taken from PrismarineJS/minecraft-data:
//! https://github.com/PrismarineJS/minecraft-data/blob/master/data/pc/1.21.1/protocol.json
//! IDs are documented here for the registry owner; Encode/Decode handles fields only.
use std::{io, sync::Arc};
use uuid::Uuid;

use crate::{
    config::GameMode,
    identity::Profile,
    protocol::{Decode, Encode, Reader, invalid, put_string, put_varint},
    world::{SPAWN_X, SPAWN_Y, SPAWN_Z, VIEW_DISTANCE, WORLD_LIMIT},
};

/// Clientbound 0x2b. A single overworld with dynamic dimension type 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinGame {
    pub entity_id: i32,
    pub view_distance: i32,
    pub game_mode: GameMode,
}

impl Encode for JoinGame {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(&self.entity_id.to_be_bytes());
        out.push(0); // Not hardcore.
        put_varint(1, out);
        put_string("minecraft:overworld", out)?;
        put_varint(20, out); // Advertised maximum players.
        put_varint(self.view_distance, out);
        put_varint(VIEW_DISTANCE, out); // No dynamic simulation yet.
        out.extend_from_slice(&[0, 1, 0]); // Reduced debug, respawn screen, limited crafting.
        put_varint(0, out); // SpawnInfo: dynamic dimension type ID.
        put_string("minecraft:overworld", out)?;
        out.extend_from_slice(&0i64.to_be_bytes()); // Hashed seed.
        out.extend_from_slice(&[self.game_mode.wire_id(), 0xff, 0, 1, 0]); // Mode, previous=-1, debug, flat, death absent.
        put_varint(0, out); // Portal cooldown.
        out.push(0); // Enforces secure chat.
        Ok(())
    }
}

/// Clientbound 0x38: game-mode abilities (including creative flight).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerAbilities(pub GameMode);

impl Encode for PlayerAbilities {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(if self.0 == GameMode::Creative {
            0x0d
        } else {
            0x07
        });
        out.extend_from_slice(&0.05f32.to_be_bytes());
        out.extend_from_slice(&0.1f32.to_be_bytes());
        Ok(())
    }
}

/// Clientbound 0x56. Packed block position x=8, y=64, z=8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultSpawnPosition;

impl Encode for DefaultSpawnPosition {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        let position = (8u64 << 38) | (8u64 << 12) | 64;
        out.extend_from_slice(&position.to_be_bytes());
        out.extend_from_slice(&0f32.to_be_bytes());
        Ok(())
    }
}

/// Clientbound 0x40. Absolute spawn position, with no relative-position flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerPosition {
    pub teleport_id: i32,
}

impl Encode for PlayerPosition {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        if self.teleport_id < 0 {
            return Err(invalid("negative teleport ID"));
        }
        for coordinate in [SPAWN_X, SPAWN_Y, SPAWN_Z] {
            out.extend_from_slice(&coordinate.to_be_bytes());
        }
        out.extend_from_slice(&0f32.to_be_bytes());
        out.extend_from_slice(&0f32.to_be_bytes());
        out.push(0);
        put_varint(self.teleport_id, out);
        Ok(())
    }
}

/// Clientbound 0x54.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CenterChunk(pub [i32; 2]);

impl Encode for CenterChunk {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        for value in self.0 {
            put_varint(value, out);
        }
        Ok(())
    }
}

/// Clientbound 0x55.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkViewDistance(pub i32);

impl Encode for ChunkViewDistance {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.0, out);
        Ok(())
    }
}

/// Clientbound 0x21 uses fixed-width Z before X (unlike ChunkData).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetChunk(pub [i32; 2]);

impl Encode for ForgetChunk {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        let [x, z] = self.0;
        out.extend_from_slice(&z.to_be_bytes());
        out.extend_from_slice(&x.to_be_bytes());
        Ok(())
    }
}

/// Clientbound 0x27. Coordinates followed by shared encoded terrain/light.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkData(pub [i32; 2], pub Arc<[u8]>);

impl Encode for ChunkData {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        for value in self.0 {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out.extend_from_slice(&self.1);
        Ok(())
    }
}

/// Clientbound 0x0d.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkBatchStart;

impl Encode for ChunkBatchStart {
    fn encode(&self, _out: &mut Vec<u8>) -> io::Result<()> {
        Ok(())
    }
}

/// Clientbound 0x0c.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkBatchFinished {
    pub count: i32,
}

impl Encode for ChunkBatchFinished {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        if self.count < 0 {
            return Err(invalid("negative chunk batch count"));
        }
        put_varint(self.count, out);
        Ok(())
    }
}

/// Clientbound 0x22, level_chunks_load_start (reason 13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameEvent;

impl Encode for GameEvent {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(13);
        out.extend_from_slice(&0f32.to_be_bytes());
        Ok(())
    }
}

/// Clientbound 0x64. Negative time freezes the client daylight cycle at noon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeUpdate;

impl Encode for TimeUpdate {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(&0i64.to_be_bytes());
        out.extend_from_slice(&(-6000i64).to_be_bytes());
        Ok(())
    }
}

/// Clientbound 0x25. World-scale border centered at the origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldBorder;

impl Encode for WorldBorder {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        for value in [0.0, 0.0, 2.0 * WORLD_LIMIT, 2.0 * WORLD_LIMIT] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        // Zero is identical in the protocol's VarLong and VarInt encodings.
        out.push(0); // Resize duration.
        put_varint(WORLD_LIMIT as i32, out);
        put_varint(5, out);
        put_varint(15, out);
        Ok(())
    }
}

/// Clientbound 0x26.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepAlive {
    pub id: i64,
}

impl Encode for KeepAlive {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(&self.id.to_be_bytes());
        Ok(())
    }
}

/// Clientbound 0x1d. Anonymous NBT TAG_String; deliberately ASCII-only reasons.
/// NUL is rejected because Java's Modified UTF-8 requires a special encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayDisconnect {
    pub reason: String,
}

impl Encode for PlayDisconnect {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        if !self.reason.is_ascii()
            || self.reason.contains('\0')
            || self.reason.len() > u16::MAX as usize
        {
            return Err(invalid(
                "disconnect reason must be non-NUL ASCII and fit NBT string",
            ));
        }
        out.push(8);
        out.extend_from_slice(&(self.reason.len() as u16).to_be_bytes());
        out.extend_from_slice(self.reason.as_bytes());
        Ok(())
    }
}

/// Clientbound 0x3e. Add one trusted profile, set spectator game mode, list player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerInfo {
    pub profile: Profile,
    pub game_mode: GameMode,
}

impl Encode for PlayerInfo {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        if self.profile.name.is_empty()
            || self.profile.name.encode_utf16().count() > 16
            || self.profile.properties.len() > 16
        {
            return Err(invalid("invalid player info profile"));
        }
        out.push(0x0d); // add_player | update_game_mode | update_listed
        put_varint(1, out);
        out.extend_from_slice(self.profile.uuid.as_bytes());
        put_string(&self.profile.name, out)?;
        put_varint(self.profile.properties.len() as i32, out);
        for property in &self.profile.properties {
            put_string(&property.name, out)?;
            put_string(&property.value, out)?;
            out.push(u8::from(property.signature.is_some()));
            if let Some(signature) = &property.signature {
                put_string(signature, out)?;
            }
        }
        put_varint(i32::from(self.game_mode.wire_id()), out);
        out.push(1); // Listed boolean (also canonical VarInt 1).
        Ok(())
    }
}

/// Serverbound 0x00.
#[derive(Debug, PartialEq, Eq)]
pub struct TeleportConfirm {
    pub id: i32,
}

impl Decode for TeleportConfirm {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let id = reader.varint()?;
        if id < 0 {
            return Err(invalid("negative teleport ID"));
        }
        Ok(Self { id })
    }
}

/// Serverbound 0x18.
#[derive(Debug, PartialEq, Eq)]
pub struct KeepAliveResponse {
    pub id: i64,
}

impl Decode for KeepAliveResponse {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self { id: reader.i64()? })
    }
}

/// Serverbound 0x1a.
#[derive(Debug, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub on_ground: bool,
}

impl Decode for Position {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            x: coordinate(reader, 30_000_000.0)?,
            y: coordinate(reader, 20_000_000.0)?,
            z: coordinate(reader, 30_000_000.0)?,
            on_ground: boolean(reader)?,
        })
    }
}

/// Serverbound 0x1b.
#[derive(Debug, PartialEq)]
pub struct PositionLook {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
}

impl Decode for PositionLook {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            x: coordinate(reader, 30_000_000.0)?,
            y: coordinate(reader, 20_000_000.0)?,
            z: coordinate(reader, 30_000_000.0)?,
            yaw: finite_float(reader)?,
            pitch: finite_float(reader)?,
            on_ground: boolean(reader)?,
        })
    }
}

/// Serverbound 0x1c.
#[derive(Debug, PartialEq)]
pub struct Look {
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
}

impl Decode for Look {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            yaw: finite_float(reader)?,
            pitch: finite_float(reader)?,
            on_ground: boolean(reader)?,
        })
    }
}

/// Serverbound 0x1d.
#[derive(Debug, PartialEq, Eq)]
pub struct Flying {
    pub on_ground: bool,
}

impl Decode for Flying {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            on_ground: boolean(reader)?,
        })
    }
}

/// Serverbound 0x08. An advisory client throughput estimate, not a server limit.
#[derive(Debug, PartialEq)]
pub struct ChunkBatchReceived {
    pub chunks_per_tick: f32,
}

impl Decode for ChunkBatchReceived {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let chunks_per_tick = finite_float(reader)?;
        if chunks_per_tick <= 0.0 {
            return Err(invalid("chunk throughput must be positive"));
        }
        Ok(Self { chunks_per_tick })
    }
}

/// Serverbound 0x23. Clients can only report the flying bit, not grant abilities.
#[derive(Debug, PartialEq, Eq)]
pub struct Abilities {
    pub flags: u8,
}

impl Decode for Abilities {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let flags = reader.take(1)?[0];
        if flags & !0x02 != 0 {
            return Err(invalid("invalid client ability flags"));
        }
        Ok(Self { flags })
    }
}

/// Serverbound 0x25.
#[derive(Debug, PartialEq, Eq)]
pub struct EntityAction {
    pub entity_id: i32,
    pub action: i32,
    pub jump_boost: i32,
}

impl Decode for EntityAction {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let packet = Self {
            entity_id: reader.varint()?,
            action: reader.varint()?,
            jump_boost: reader.varint()?,
        };
        if packet.entity_id < 0
            || !(0..=8).contains(&packet.action)
            || !(0..=100).contains(&packet.jump_boost)
        {
            return Err(invalid("invalid entity action"));
        }
        Ok(packet)
    }
}

/// Serverbound 0x2f.
#[derive(Debug, PartialEq, Eq)]
pub struct HeldItemSlot {
    pub slot: i16,
}

impl Decode for HeldItemSlot {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let slot = i16::from_be_bytes(reader.take(2)?.try_into().unwrap());
        if !(0..=8).contains(&slot) {
            return Err(invalid("invalid hotbar slot"));
        }
        Ok(Self { slot })
    }
}

/// Serverbound 0x36.
#[derive(Debug, PartialEq, Eq)]
pub struct ArmAnimation {
    pub hand: i32,
}

impl Decode for ArmAnimation {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let hand = reader.varint()?;
        if !(0..=1).contains(&hand) {
            return Err(invalid("invalid hand"));
        }
        Ok(Self { hand })
    }
}

/// Serverbound 0x07. Parsed only; this does not authenticate or enable chat.
#[derive(Debug, PartialEq, Eq)]
pub struct ChatSessionUpdate {
    pub session_uuid: Uuid,
    pub expires_at: i64,
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

impl Decode for ChatSessionUpdate {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let session_uuid = Uuid::from_bytes(reader.take(16)?.try_into().unwrap());
        let expires_at = reader.i64()?;
        // 1.21.1 serverbound profile keys/signatures are bounded to 512/4096 bytes.
        let public_key = reader.byte_array(512)?;
        let signature = reader.byte_array(4096)?;
        if public_key.is_empty() || signature.is_empty() {
            return Err(invalid("empty chat session key or signature"));
        }
        Ok(Self {
            session_uuid,
            expires_at,
            public_key,
            signature,
        })
    }
}

fn boolean(reader: &mut Reader<'_>) -> io::Result<bool> {
    match reader.take(1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid boolean")),
    }
}

fn finite_float(reader: &mut Reader<'_>) -> io::Result<f32> {
    let value = f32::from_be_bytes(reader.take(4)?.try_into().unwrap());
    if !value.is_finite() {
        return Err(invalid("non-finite float"));
    }
    Ok(value)
}

fn coordinate(reader: &mut Reader<'_>, limit: f64) -> io::Result<f64> {
    let value = f64::from_be_bytes(reader.take(8)?.try_into().unwrap());
    if !value.is_finite() || value.abs() > limit {
        return Err(invalid("invalid player coordinate"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
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
        let bytes = encoded(&PlayerPosition { teleport_id: 123 });
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
        assert_eq!(encoded(&GameEvent), [13, 0, 0, 0, 0]);
        let bytes = encoded(&TimeUpdate);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.i64().unwrap(), 0);
        assert_eq!(reader.i64().unwrap(), -6000);
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
        assert!(
            PlayerPosition { teleport_id: -1 }
                .encode(&mut Vec::new())
                .is_err()
        );
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
}
