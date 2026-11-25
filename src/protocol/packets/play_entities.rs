//! Player entity packets for Java 1.21.1 (protocol 767).
use crate::{
    game::ItemStack,
    protocol::{Encode, packets::play_blocks::put_item_stack, put_varint},
};
use std::io;
use uuid::Uuid;

// minecraft-data 1.21.1 entity registry.
const PLAYER_ENTITY_TYPE: i32 = 128;
const ITEM_ENTITY_TYPE: i32 = 58;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityPose {
    // Store the wire bits for structural equality, not float equality.
    pub(crate) position: [u64; 3],
    pub yaw: u8,
    pub pitch: u8,
    // Preserve authoritative angles for selectors; wire angles are quantized.
    pub(crate) rotation: [u32; 2],
    pub on_ground: bool,
}

impl EntityPose {
    pub(crate) fn new(position: [f64; 3], yaw: f32, pitch: f32, on_ground: bool) -> Self {
        // Angles wrap at 360°, and floor matches the vanilla packed-angle rule.
        let angle = |value: f32| {
            (f64::from(value).rem_euclid(360.0) * (256.0 / 360.0)).floor() as u16 as u8
        };
        Self {
            position: position.map(f64::to_bits),
            yaw: angle(yaw),
            pitch: angle(pitch),
            rotation: [yaw.to_bits(), pitch.to_bits()],
            on_ground,
        }
    }

    fn encode_position(&self, out: &mut Vec<u8>) {
        for coordinate in self.position {
            out.extend_from_slice(&coordinate.to_be_bytes());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SpawnPlayer {
    pub id: i32,
    pub uuid: Uuid,
    pub pose: EntityPose,
}

impl Encode for SpawnPlayer {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.id, out);
        out.extend_from_slice(self.uuid.as_bytes());
        put_varint(PLAYER_ENTITY_TYPE, out);
        self.pose.encode_position(out);
        out.extend_from_slice(&[self.pose.pitch, self.pose.yaw, self.pose.yaw, 0]);
        out.extend_from_slice(&[0; 6]); // Velocity, three signed shorts.
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RemovePlayerInfo(pub Vec<Uuid>);

impl Encode for RemovePlayerInfo {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.0.len().try_into().map_err(io::Error::other)?, out);
        for uuid in &self.0 {
            out.extend_from_slice(uuid.as_bytes());
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RemoveEntities(pub Vec<i32>);

impl Encode for RemoveEntities {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.0.len().try_into().map_err(io::Error::other)?, out);
        for id in &self.0 {
            put_varint(*id, out);
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EntityHeadRotation {
    pub id: i32,
    pub yaw: u8,
}

impl Encode for EntityHeadRotation {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.id, out);
        out.push(self.yaw);
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EntityTeleport {
    pub id: i32,
    pub pose: EntityPose,
}

impl Encode for EntityTeleport {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.id, out);
        self.pose.encode_position(out);
        out.extend_from_slice(&[
            self.pose.yaw,
            self.pose.pitch,
            u8::from(self.pose.on_ground),
        ]);
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EntityLook {
    pub id: i32,
    pub pose: EntityPose,
}

impl Encode for EntityLook {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.id, out);
        out.extend_from_slice(&[
            self.pose.yaw,
            self.pose.pitch,
            u8::from(self.pose.on_ground),
        ]);
        Ok(())
    }
}

/// Clientbound 0x03: Entity Animation (`0` = swing main arm, `3` = swing offhand).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityAnimation {
    pub id: i32,
    pub animation: u8,
}

impl Encode for EntityAnimation {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.id, out);
        out.push(self.animation);
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpawnEntity {
    Player(SpawnPlayer),
    Item {
        id: i32,
        uuid: Uuid,
        position_bits: [u64; 3],
        velocity_scaled: [i16; 3],
    },
}

impl SpawnEntity {
    pub fn item(id: i32, uuid: Uuid, position: [f64; 3], velocity: [f64; 3]) -> Self {
        Self::Item {
            id,
            uuid,
            position_bits: position.map(f64::to_bits),
            velocity_scaled: velocity.map(|v| (v.clamp(-3.9, 3.9) * 8000.0).round() as i16),
        }
    }
}

impl Encode for SpawnEntity {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        match self {
            Self::Player(player) => player.encode(out),
            Self::Item {
                id,
                uuid,
                position_bits,
                velocity_scaled,
            } => {
                put_varint(*id, out);
                out.extend_from_slice(uuid.as_bytes());
                put_varint(ITEM_ENTITY_TYPE, out);
                for coord in position_bits {
                    out.extend_from_slice(&coord.to_be_bytes());
                }
                out.extend_from_slice(&[0, 0, 0, 0]); // pitch, yaw, head_yaw, data
                for scaled in velocity_scaled {
                    out.extend_from_slice(&scaled.to_be_bytes());
                }
                Ok(())
            }
        }
    }
}

/// Clientbound 0x5a: Set Entity Velocity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityVelocity {
    id: i32,
    velocity_scaled: [i16; 3],
}

impl EntityVelocity {
    pub fn new(id: i32, velocity: [f64; 3]) -> Self {
        Self {
            id,
            velocity_scaled: velocity.map(|v| (v.clamp(-3.9, 3.9) * 8000.0).round() as i16),
        }
    }
}

impl Encode for EntityVelocity {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.id, out);
        for scaled in self.velocity_scaled {
            out.extend_from_slice(&scaled.to_be_bytes());
        }
        Ok(())
    }
}

/// Clientbound 0x6f: Pickup Item animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectItem {
    pub collected_id: i32,
    pub collector_id: i32,
    pub count: i32,
}

impl Encode for CollectItem {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.collected_id, out);
        put_varint(self.collector_id, out);
        put_varint(self.count, out);
        Ok(())
    }
}

/// Clientbound 0x58: Set Entity Metadata (player sneak/sprint or dropped item stack).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityMetadata {
    Player {
        id: i32,
        sneaking: bool,
        sprinting: bool,
    },
    Item {
        id: i32,
        stack: ItemStack,
    },
}

impl Encode for EntityMetadata {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        match *self {
            Self::Player {
                id,
                sneaking,
                sprinting,
            } => {
                put_varint(id, out);
                // Index 0: Entity shared flags (type 0 = Byte).
                let flags = (u8::from(sneaking) << 1) | (u8::from(sprinting) << 3);
                out.push(0);
                put_varint(0, out);
                out.push(flags);
                // Index 6: Entity Pose (type 21 = Pose VarInt; 0 = STANDING, 5 = CROUCHING).
                out.push(6);
                put_varint(21, out);
                put_varint(if sneaking { 5 } else { 0 }, out);
                out.push(0xff);
            }
            Self::Item { id, stack } => {
                put_varint(id, out);
                // Index 8: ItemEntity item stack (type 7 = Slot).
                out.push(8);
                put_varint(7, out);
                put_item_stack(Some(stack), out);
                out.push(0xff);
            }
        }
        Ok(())
    }
}

/// Clientbound 0x5b: Set Equipment (main hand slot 0 + offhand slot 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityEquipment {
    pub id: i32,
    pub main_hand: Option<ItemStack>,
    pub off_hand: Option<ItemStack>,
}

impl Encode for EntityEquipment {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.id, out);
        // Top-bit-set-terminated array: bit 7 set on slot 0 (more follows), clear on slot 1.
        out.push(0x80); // slot 0 (main hand) | 0x80
        put_item_stack(self.main_hand, out);
        out.push(0x01); // slot 1 (offhand)
        put_item_stack(self.off_hand, out);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Clientbound;

    #[test]
    fn protocol_767_entity_fields_and_angle_wrap() {
        let pose = EntityPose::new([8.5, 65.0, -1.0], 270.0, -45.0, true);
        assert_eq!([pose.yaw, pose.pitch], [192, 224]);
        assert_eq!(EntityPose::new([0.0; 3], -0.5, 450.0, false).yaw, 255);
        let uuid = Uuid::from_bytes([0xAB; 16]);
        let encode = |packet: Clientbound| packet.encode().unwrap();
        let spawn = encode(
            SpawnEntity::Player(SpawnPlayer {
                id: 300,
                uuid,
                pose,
            })
            .into(),
        );
        assert_eq!(&spawn[..3], &[0x01, 0xAC, 2]);
        assert_eq!(&spawn[3..19], uuid.as_bytes());
        assert_eq!(&spawn[19..21], &[0x80, 1]);
        assert_eq!(
            &spawn[21..45],
            &pose.position.map(u64::to_be_bytes).concat()
        );
        assert_eq!(&spawn[45..], &[224, 192, 192, 0, 0, 0, 0, 0, 0, 0]);
        let teleport = encode(EntityTeleport { id: 300, pose }.into());
        assert_eq!(&teleport[..3], &[0x70, 0xAC, 2]);
        assert_eq!(&teleport[3..27], &spawn[21..45]);
        assert_eq!(&teleport[27..], &[192, 224, 1]);
        assert_eq!(
            encode(EntityLook { id: 300, pose }.into()),
            [0x30, 0xAC, 2, 192, 224, 1]
        );
        assert_eq!(
            encode(EntityHeadRotation { id: 300, yaw: 192 }.into()),
            [0x48, 0xAC, 2, 192]
        );
        assert_eq!(encode(RemoveEntities(vec![300]).into()), [0x42, 1, 0xAC, 2]);
        let remove = encode(RemovePlayerInfo(vec![uuid]).into());
        assert_eq!(&remove[..2], &[0x3d, 1]);
        assert_eq!(&remove[2..], uuid.as_bytes());

        // EntityAnimation (0x03), EntityMetadata (0x58), EntityEquipment (0x5b).
        assert_eq!(
            encode(
                EntityAnimation {
                    id: 5,
                    animation: 0
                }
                .into()
            ),
            [0x03, 5, 0]
        );
        assert_eq!(
            encode(
                EntityMetadata::Player {
                    id: 5,
                    sneaking: true,
                    sprinting: true,
                }
                .into()
            ),
            [0x58, 5, 0, 0, 0x0a, 6, 21, 5, 0xff]
        );
        let eq = encode(
            EntityEquipment {
                id: 5,
                main_hand: None,
                off_hand: None,
            }
            .into(),
        );
        assert_eq!(eq, [0x5b, 5, 0x80, 0, 0x01, 0]);
    }
}
