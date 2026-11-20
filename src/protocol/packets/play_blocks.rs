//! Block interaction and inventory packets for Java 1.21.1 / protocol 767.
use crate::{
    protocol::{Decode, Encode, Reader, invalid, put_varint},
    world::BlockPos,
};
use std::{io, sync::Arc};

fn position(r: &mut Reader<'_>) -> io::Result<BlockPos> {
    let v = r.i64()?;
    Ok(BlockPos([
        (v >> 38) as i32,
        (v << 52 >> 52) as i32,
        (v << 26 >> 38) as i32,
    ]))
}
fn sequence(r: &mut Reader<'_>) -> io::Result<i32> {
    let n = r.varint()?;
    if n < 0 {
        return Err(invalid("negative block prediction sequence"));
    }
    Ok(n)
}

#[derive(Debug, PartialEq)]
pub struct PlayerDigging {
    pub action: i32,
    pub pos: BlockPos,
    pub face: u8,
    pub sequence: i32,
}
impl Decode for PlayerDigging {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            action: r.varint()?,
            pos: position(r)?,
            face: r.take(1)?[0],
            sequence: sequence(r)?,
        })
    }
}

#[derive(Debug, PartialEq)]
pub struct BlockPlace {
    pub hand: i32,
    pub pos: BlockPos,
    pub face: i32,
    pub hit: [f32; 3],
    pub sequence: i32,
}
impl Decode for BlockPlace {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        let hand = r.varint()?;
        let pos = position(r)?;
        let face = r.varint()?;
        let mut hit = [0.; 3];
        for component in &mut hit {
            *component = f32::from_be_bytes(r.take(4)?.try_into().unwrap());
        }
        if r.take(1)?[0] > 1 {
            return Err(invalid("invalid inside-block flag"));
        }
        Ok(Self {
            hand,
            pos,
            face,
            hit,
            sequence: sequence(r)?,
        })
    }
}

#[derive(Debug, PartialEq)]
pub struct CreativeSlot;
impl Decode for CreativeSlot {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        r.u16()?;
        // Reject changes as a policy. Never deserialize user-provided components.
        r.remaining(16_384)?;
        Ok(Self)
    }
}

#[derive(Debug, PartialEq)]
pub struct UseItem(pub i32);
impl Decode for UseItem {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        r.varint()?;
        let id = sequence(r)?;
        r.take(8)?;
        Ok(Self(id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockAck(pub i32);
impl Encode for BlockAck {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.0, out);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockUpdate(pub BlockPos, pub i32);
impl Encode for BlockUpdate {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        let [x, y, z] = self.0.0;
        let packed =
            ((x as u64 & 0x3ff_ffff) << 38) | ((z as u64 & 0x3ff_ffff) << 12) | (y as u64 & 0xfff);
        out.extend(packed.to_be_bytes());
        put_varint(self.1, out);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightUpdate(pub [i32; 2], pub Arc<[u8]>);
impl Encode for LightUpdate {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        for c in self.0 {
            put_varint(c, out);
        }
        out.extend_from_slice(&self.1);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreativeHotbar;
impl Encode for CreativeHotbar {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend([0, 0]); // Player inventory container and state ID.
        put_varint(46, out);
        for slot in 0usize..46 {
            if let Some(&(id, _)) = slot
                .checked_sub(36)
                .and_then(|i| crate::world::HOTBAR.get(i))
            {
                out.push(64);
                put_varint(id, out);
                out.extend([0, 0]); // No component overrides.
            } else {
                out.push(0);
            }
        }
        out.push(0); // Empty cursor stack.
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedSlot(pub u8);
impl Encode for SelectedSlot {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(self.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_position_round_trip_and_prediction_sequence() {
        let pos = BlockPos([-29_999_984, -64, 29_999_984]);
        let mut buf = Vec::new();
        BlockUpdate(pos, 519).encode(&mut buf).unwrap();
        let mut r = Reader::new(&buf);
        assert_eq!(position(&mut r).unwrap(), pos);
        assert_eq!(r.varint().unwrap(), 519);
        r.finish().unwrap();
        let mut ack = Vec::new();
        BlockAck(300).encode(&mut ack).unwrap();
        assert_eq!(ack, [0xac, 2]);
    }
}
