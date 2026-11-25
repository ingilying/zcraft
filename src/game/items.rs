use super::{ItemDefinition, item_by_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hand {
    Main,
    Off,
}
impl Hand {
    pub fn from_wire(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Main),
            1 => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStack {
    pub item: i32,
    pub count: u8,
    pub damage: u32,
}
impl ItemStack {
    pub fn new(item: i32, count: u8) -> Option<Self> {
        let definition = item_by_id(item)?;
        (count > 0 && count <= definition.max_stack).then_some(Self {
            item,
            count,
            damage: 0,
        })
    }
    pub fn with_damage(item: i32, count: u8, damage: u32) -> Option<Self> {
        let definition = item_by_id(item)?;
        (count > 0 && count <= definition.max_stack && damage < definition.max_damage.max(1))
            .then_some(Self {
                item,
                count,
                damage,
            })
    }
    pub fn definition(self) -> &'static ItemDefinition {
        item_by_id(self.item).expect("validated item stack")
    }
    /// Returns true when the stack's sole tool breaks.
    pub fn damage_one(&mut self) -> bool {
        let max = self.definition().max_damage;
        if max == 0 {
            return false;
        }
        self.damage += 1;
        self.damage >= max
    }
}
