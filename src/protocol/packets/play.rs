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
    position_bits: [u64; 3],
    yaw_bits: u32,
    pitch_bits: u32,
}

impl PlayerPosition {
    pub fn spawn(teleport_id: i32) -> Self {
        Self::at([SPAWN_X, SPAWN_Y, SPAWN_Z], 0.0, 0.0, teleport_id)
    }

    pub fn at(position: [f64; 3], yaw: f32, pitch: f32, teleport_id: i32) -> Self {
        Self {
            teleport_id,
            position_bits: position.map(f64::to_bits),
            yaw_bits: yaw.to_bits(),
            pitch_bits: pitch.to_bits(),
        }
    }
}

impl Encode for PlayerPosition {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        if self.teleport_id < 0 {
            return Err(invalid("negative teleport ID"));
        }
        for coordinate in self.position_bits {
            out.extend_from_slice(&coordinate.to_be_bytes());
        }
        out.extend_from_slice(&self.yaw_bits.to_be_bytes());
        out.extend_from_slice(&self.pitch_bits.to_be_bytes());
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

/// Clientbound 0x22 (`game_event`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameEvent {
    pub event: u8,
    value_bits: u32,
}

impl GameEvent {
    pub const START_RAINING: u8 = 1;
    pub const STOP_RAINING: u8 = 2;
    pub const CHANGE_GAME_MODE: u8 = 3;
    pub const RAIN_LEVEL_CHANGE: u8 = 7;
    pub const THUNDER_LEVEL_CHANGE: u8 = 8;
    pub const LEVEL_CHUNKS_LOAD_START: u8 = 13;

    pub fn new(event: u8, value: f32) -> Self {
        Self {
            event,
            value_bits: value.to_bits(),
        }
    }

    pub fn value(self) -> f32 {
        f32::from_bits(self.value_bits)
    }

    pub fn level_chunks_load_start() -> Self {
        Self::new(Self::LEVEL_CHUNKS_LOAD_START, 0.0)
    }

    pub fn change_game_mode(mode: GameMode) -> Self {
        Self::new(Self::CHANGE_GAME_MODE, f32::from(mode.wire_id()))
    }

    pub fn start_raining() -> Self {
        Self::new(Self::START_RAINING, 0.0)
    }

    pub fn stop_raining() -> Self {
        Self::new(Self::STOP_RAINING, 0.0)
    }

    pub fn rain_level(level: f32) -> Self {
        Self::new(Self::RAIN_LEVEL_CHANGE, level.clamp(0.0, 1.0))
    }

    pub fn thunder_level(level: f32) -> Self {
        Self::new(Self::THUNDER_LEVEL_CHANGE, level.clamp(0.0, 1.0))
    }
}

impl Encode for GameEvent {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(self.event);
        out.extend_from_slice(&self.value_bits.to_be_bytes());
        Ok(())
    }
}

/// Clientbound 0x64. Positive `time_of_day` advances on the client; negative freezes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeUpdate {
    pub world_age: i64,
    pub time_of_day: i64,
}

impl Default for TimeUpdate {
    fn default() -> Self {
        Self {
            world_age: 0,
            time_of_day: crate::world::INITIAL_DAY_TIME,
        }
    }
}

impl Encode for TimeUpdate {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(&self.world_age.to_be_bytes());
        out.extend_from_slice(&self.time_of_day.to_be_bytes());
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
#[path = "play_tests.rs"]
mod tests;
