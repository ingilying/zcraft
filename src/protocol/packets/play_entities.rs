//! Player entity packets for Java 1.21.1 (protocol 767).
use crate::protocol::{Encode, put_varint};
use std::io;
use uuid::Uuid;

// minecraft-data 1.21.1 entity registry. Players use Add Entity in 767.
const PLAYER_ENTITY_TYPE: i32 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityPose {
    // Store the wire bits for structural equality, not float equality.
    pub(crate) position: [u64; 3],
    pub yaw: u8,
    pub pitch: u8,
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
            SpawnPlayer {
                id: 300,
                uuid,
                pose,
            }
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
    }
}
