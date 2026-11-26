//! Block interaction and inventory packets for Java 1.21.1 / protocol 767.
use crate::{
    game::{Inventory, InventoryClick, Item, ItemStack},
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingStack {
    Empty,
    Supported(ItemStack),
    Unsupported,
}
impl IncomingStack {
    pub fn supported(self) -> Option<Option<ItemStack>> {
        match self {
            Self::Empty => Some(None),
            Self::Supported(stack) => Some(Some(stack)),
            Self::Unsupported => None,
        }
    }
}
fn item_stack(r: &mut Reader<'_>) -> io::Result<IncomingStack> {
    let count = r.varint()?;
    if count == 0 {
        return Ok(IncomingStack::Empty);
    }
    if !(1..=64).contains(&count) {
        return Err(invalid("invalid item count"));
    }
    let id = r.varint()?;
    let added = r.varint()?;
    if !(0..=64).contains(&added) {
        return Err(invalid("invalid added component count"));
    }
    let removed = r.varint()?;
    if !(0..=64).contains(&removed) {
        return Err(invalid("invalid removed component count"));
    }
    let mut damage = 0;
    for _ in 0..added {
        match r.varint()? {
            3 => {
                let value = r.varint()?;
                if value < 0 {
                    return Err(invalid("negative item damage"));
                }
                damage = value as u32;
            }
            _ => return Err(invalid("unsupported item component")),
        }
    }
    for _ in 0..removed {
        r.varint()?;
    }
    if removed != 0 {
        return Err(invalid("removed item components are not supported"));
    }
    let Some(item) = Item::from_wire_id(id) else {
        return Ok(IncomingStack::Unsupported);
    };
    Ok(ItemStack::with_damage(item, count as u8, damage)
        .map_or(IncomingStack::Unsupported, IncomingStack::Supported))
}
pub(crate) fn put_item_stack(stack: Option<ItemStack>, out: &mut Vec<u8>) {
    if let Some(stack) = stack {
        put_varint(i32::from(stack.count), out);
        put_varint(stack.item.wire_id(), out);
        put_varint(i32::from(stack.damage > 0), out); // addedComponentCount
        put_varint(0, out); // removedComponentCount
        if stack.damage > 0 {
            put_varint(3, out); // minecraft:damage
            put_varint(stack.damage as i32, out);
        }
    } else {
        put_varint(0, out)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CreativeSlot {
    pub slot: i16,
    pub item: IncomingStack,
}
impl Decode for CreativeSlot {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            slot: r.i16()?,
            item: item_stack(r)?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct InventoryClickPacket {
    pub window: u8,
    pub state_id: i32,
    pub click: InventoryClick,
}
impl Decode for InventoryClickPacket {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        let packet = Self {
            window: r.u8()?,
            state_id: r.varint()?,
            click: InventoryClick {
                slot: r.i16()?,
                button: r.i8()?,
                mode: r.varint()?,
            },
        };
        let changed = r.varint()?;
        if !(0..=128).contains(&changed) {
            return Err(invalid("invalid changed-slot count"));
        }
        for _ in 0..changed {
            r.i16()?;
            item_stack(r)?;
        }
        item_stack(r)?;
        Ok(packet)
    }
}

#[derive(Debug, PartialEq)]
pub struct UseItem {
    pub hand: i32,
    pub sequence: i32,
}
impl Decode for UseItem {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        let hand = r.varint()?;
        let sequence = sequence(r)?;
        r.take(8)?;
        Ok(Self { hand, sequence })
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
pub struct CloseWindow {
    pub window_id: u8,
}
impl Decode for CloseWindow {
    fn decode(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self { window_id: r.u8()? })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockAction {
    pub pos: BlockPos,
    pub action_id: u8,
    pub action_param: u8,
    pub block_id: i32,
}
impl Encode for BlockAction {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        let [x, y, z] = self.pos.0;
        let packed =
            ((x as u64 & 0x3ff_ffff) << 38) | ((z as u64 & 0x3ff_ffff) << 12) | (y as u64 & 0xfff);
        out.extend(packed.to_be_bytes());
        out.push(self.action_id);
        out.push(self.action_param);
        put_varint(self.block_id, out);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseContainer(pub u8);
impl Encode for CloseContainer {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(self.0);
        Ok(())
    }
}

/// Clientbound 0x14: Set Container Property (e.g. Furnace burn/cook progress).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerSetData {
    pub window_id: u8,
    pub property: i16,
    pub value: i16,
}
impl Encode for ContainerSetData {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(self.window_id);
        out.extend_from_slice(&self.property.to_be_bytes());
        out.extend_from_slice(&self.value.to_be_bytes());
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenScreen {
    pub window_id: i32,
    pub menu_type: i32,
    pub title: String,
}
impl Encode for OpenScreen {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.window_id, out);
        put_varint(self.menu_type, out);
        // Anonymous NBT TAG_String (type 8) for plain text component.
        out.push(8);
        let bytes = self.title.as_bytes();
        out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        out.extend_from_slice(bytes);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryContent {
    pub window_id: u8,
    pub state_id: i32,
    pub slots: Vec<Option<ItemStack>>,
    pub carried: Option<ItemStack>,
}
impl InventoryContent {
    pub fn player(inv: &Inventory) -> Self {
        Self {
            window_id: 0,
            state_id: inv.state_id(),
            slots: inv.slots().to_vec(),
            carried: inv.carried(),
        }
    }
    pub fn container(
        window_id: u8,
        state_id: i32,
        slots: Vec<Option<ItemStack>>,
        carried: Option<ItemStack>,
    ) -> Self {
        Self {
            window_id,
            state_id,
            slots,
            carried,
        }
    }
}
impl Encode for InventoryContent {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(self.window_id);
        put_varint(self.state_id, out);
        put_varint(self.slots.len() as i32, out);
        for stack in &self.slots {
            put_item_stack(*stack, out)
        }
        put_item_stack(self.carried, out);
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetInventorySlot {
    pub state_id: i32,
    pub slot: i16,
    pub item: Option<ItemStack>,
}
impl Encode for SetInventorySlot {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        out.push(0);
        put_varint(self.state_id, out);
        out.extend(self.slot.to_be_bytes());
        put_item_stack(self.item, out);
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

    #[test]
    fn inventory_slots_are_bounded_typed_and_component_free() {
        let inventory = Inventory::default();
        let mut body = Vec::new();
        InventoryContent::player(&inventory)
            .encode(&mut body)
            .unwrap();
        let mut r = Reader::new(&body);
        assert_eq!(r.u8().unwrap(), 0);
        assert_eq!(r.varint().unwrap(), 0);
        assert_eq!(r.varint().unwrap(), crate::game::SLOT_COUNT as i32);
        for (index, expected) in inventory.slots().iter().enumerate() {
            let decoded = item_stack(&mut r).unwrap().supported().unwrap();
            assert_eq!(decoded, *expected, "slot {index}");
        }
        assert_eq!(item_stack(&mut r).unwrap(), IncomingStack::Empty);
        r.finish().unwrap();

        let mut known = Vec::new();
        known.extend(36i16.to_be_bytes());
        let stone = Item::by_name("stone").unwrap();
        put_item_stack(ItemStack::new(stone, 12), &mut known);
        let mut r = Reader::new(&known);
        assert_eq!(
            CreativeSlot::decode(&mut r).unwrap().item,
            IncomingStack::Supported(ItemStack::new(stone, 12).unwrap())
        );
        r.finish().unwrap();

        let mut unknown = Vec::new();
        unknown.extend(36i16.to_be_bytes());
        for value in [1, 9999, 0, 0] {
            put_varint(value, &mut unknown);
        }
        assert_eq!(
            CreativeSlot::decode(&mut Reader::new(&unknown))
                .unwrap()
                .item,
            IncomingStack::Unsupported
        );

        let mut damaged = Vec::new();
        damaged.extend(39i16.to_be_bytes());
        let pickaxe = Item::by_name("wooden_pickaxe").unwrap();
        let damaged_stack = ItemStack::with_damage(pickaxe, 1, 7).unwrap();
        put_item_stack(Some(damaged_stack), &mut damaged);
        // Verify exact protocol 767 Slot field order: count, id, added, removed, [type=3, value=7].
        let mut expected_wire = Vec::new();
        expected_wire.extend(39i16.to_be_bytes());
        for v in [1, pickaxe.wire_id(), 1, 0, 3, 7] {
            put_varint(v, &mut expected_wire);
        }
        assert_eq!(damaged, expected_wire);
        let mut r = Reader::new(&damaged);
        assert_eq!(
            CreativeSlot::decode(&mut r).unwrap().item,
            IncomingStack::Supported(damaged_stack)
        );
        r.finish().unwrap();

        let mut component = Vec::new();
        component.extend(36i16.to_be_bytes());
        for value in [1, 1, 1, 0] {
            put_varint(value, &mut component);
        }
        assert!(CreativeSlot::decode(&mut Reader::new(&component)).is_err());

        let mut click = vec![0]; // Player inventory container.
        put_varint(7, &mut click);
        click.extend(38i16.to_be_bytes());
        click.push(0);
        put_varint(0, &mut click);
        put_varint(1, &mut click);
        click.extend(38i16.to_be_bytes());
        put_item_stack(None, &mut click);
        put_item_stack(
            ItemStack::new(Item::by_name("glass").unwrap(), 64),
            &mut click,
        );
        let packet = InventoryClickPacket::decode(&mut Reader::new(&click)).unwrap();
        assert_eq!(packet.window, 0);
        assert_eq!(packet.state_id, 7);
        assert_eq!(
            packet.click,
            InventoryClick {
                slot: 38,
                button: 0,
                mode: 0
            }
        );
    }
}
