use super::BlockKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Stone,
    Dirt,
    Glass,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemUse {
    None,
    Place(BlockKind),
}
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
pub struct ItemDefinition {
    pub kind: ItemKind,
    pub name: &'static str,
    pub protocol_id: i32,
    pub max_stack: u8,
    pub use_action: ItemUse,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStack {
    pub item: ItemKind,
    pub count: u8,
}
impl ItemStack {
    pub fn new(item: ItemKind, count: u8) -> Option<Self> {
        let max = item.definition().max_stack;
        (count > 0 && count <= max).then_some(Self { item, count })
    }
    pub fn definition(self) -> &'static ItemDefinition {
        self.item.definition()
    }
}
impl ItemKind {
    pub fn definition(self) -> &'static ItemDefinition {
        &ITEMS[self as usize]
    }
}

pub const ITEMS: [ItemDefinition; 3] = [
    ItemDefinition {
        kind: ItemKind::Stone,
        name: "stone",
        protocol_id: 1,
        max_stack: 64,
        use_action: ItemUse::Place(BlockKind::Stone),
    },
    ItemDefinition {
        kind: ItemKind::Dirt,
        name: "dirt",
        protocol_id: 28,
        max_stack: 64,
        use_action: ItemUse::Place(BlockKind::Dirt),
    },
    ItemDefinition {
        kind: ItemKind::Glass,
        name: "glass",
        protocol_id: 188,
        max_stack: 64,
        use_action: ItemUse::Place(BlockKind::Glass),
    },
];
pub fn items() -> &'static [ItemDefinition] {
    &ITEMS
}
pub fn item_by_id(id: i32) -> Option<&'static ItemDefinition> {
    ITEMS.iter().find(|item| item.protocol_id == id)
}
pub fn item_by_name(name: &str) -> Option<&'static ItemDefinition> {
    ITEMS.iter().find(|item| item.name == name)
}
