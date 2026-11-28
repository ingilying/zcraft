use super::{ItemDefinition, item_by_id, item_by_name};
use std::{fmt, hash};

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

/// Strongly-typed handle to a registered Minecraft item definition
/// (mirroring vanilla's `Holder<Item>`).
///
/// Gameplay logic identifies items by this handle (backed by static registry
/// identity and namespaced resource IDs); protocol-versioned numeric wire IDs
/// (`wire_id` / `from_wire_id`) are confined strictly to network codecs.
#[derive(Clone, Copy)]
pub struct Item(&'static ItemDefinition);

impl Item {
    /// Resolve an item by namespaced or short name (e.g. `"minecraft:stone"` or `"stone"`).
    pub fn by_name(name: &str) -> Option<Self> {
        item_by_name(name).map(Self)
    }

    /// Resolve an item from a protocol-767 wire ID (network boundary only).
    pub fn from_wire_id(id: i32) -> Option<Self> {
        item_by_id(id).map(Self)
    }

    /// Wrap an already-resolved static `ItemDefinition`.
    pub const fn from_def(def: &'static ItemDefinition) -> Self {
        Self(def)
    }

    /// Protocol-767 numeric wire ID (network boundary only).
    pub const fn wire_id(self) -> i32 {
        self.0.id
    }

    /// Canonical namespaced resource identifier (e.g. `"minecraft:diamond_pickaxe"`).
    pub fn name(self) -> &'static str {
        &self.0.name
    }

    pub const fn definition(self) -> &'static ItemDefinition {
        self.0
    }
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for Item {}

impl hash::Hash for Item {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(self.0, state);
    }
}

impl fmt::Debug for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Item({:?})", self.0.name)
    }
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStack {
    pub item: Item,
    pub count: u8,
    pub damage: u32,
}
impl ItemStack {
    pub fn new(item: Item, count: u8) -> Option<Self> {
        let definition = item.definition();
        (count > 0 && count <= definition.max_stack).then_some(Self {
            item,
            count,
            damage: 0,
        })
    }
    pub fn with_damage(item: Item, count: u8, damage: u32) -> Option<Self> {
        let definition = item.definition();
        (count > 0 && count <= definition.max_stack && damage < definition.max_damage.max(1))
            .then_some(Self {
                item,
                count,
                damage,
            })
    }
    pub const fn definition(self) -> &'static ItemDefinition {
        self.item.definition()
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
