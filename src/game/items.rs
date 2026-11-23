use super::{BlockDefinition, BlockKind, MiningLevel, ToolKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Stone,
    Dirt,
    Glass,
    WoodenPickaxe,
    StonePickaxe,
    IronPickaxe,
    WoodenShovel,
    StoneShovel,
    IronShovel,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockItem {
    pub block: BlockKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolItem {
    pub kind: ToolKind,
    pub level: MiningLevel,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemCategory {
    Block(BlockItem),
    Tool(ToolItem),
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
    pub category: ItemCategory,
}
impl ItemDefinition {
    /// Shared behavior inherited by every block-category item.
    pub const fn placed_block(self) -> Option<BlockKind> {
        match self.category {
            ItemCategory::Block(item) => Some(item.block),
            ItemCategory::Tool(_) => None,
        }
    }
    /// Shared behavior inherited by every tool-category item.
    pub fn can_mine(self, block: &BlockDefinition) -> bool {
        match (self.category, block.mining) {
            (ItemCategory::Tool(tool), Some(required)) => {
                tool.kind == required.tool && tool.level >= required.level
            }
            _ => false,
        }
    }
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

const fn block(
    kind: ItemKind,
    name: &'static str,
    protocol_id: i32,
    placed: BlockKind,
) -> ItemDefinition {
    ItemDefinition {
        kind,
        name,
        protocol_id,
        max_stack: 64,
        category: ItemCategory::Block(BlockItem { block: placed }),
    }
}
const fn tool(
    kind: ItemKind,
    name: &'static str,
    protocol_id: i32,
    tool: ToolKind,
    level: MiningLevel,
) -> ItemDefinition {
    ItemDefinition {
        kind,
        name,
        protocol_id,
        max_stack: 1,
        category: ItemCategory::Tool(ToolItem { kind: tool, level }),
    }
}
pub const ITEMS: [ItemDefinition; 9] = [
    block(ItemKind::Stone, "stone", 1, BlockKind::Stone),
    block(ItemKind::Dirt, "dirt", 28, BlockKind::Dirt),
    block(ItemKind::Glass, "glass", 188, BlockKind::Glass),
    tool(
        ItemKind::WoodenPickaxe,
        "wooden_pickaxe",
        820,
        ToolKind::Pickaxe,
        MiningLevel::Wood,
    ),
    tool(
        ItemKind::StonePickaxe,
        "stone_pickaxe",
        825,
        ToolKind::Pickaxe,
        MiningLevel::Stone,
    ),
    tool(
        ItemKind::IronPickaxe,
        "iron_pickaxe",
        835,
        ToolKind::Pickaxe,
        MiningLevel::Iron,
    ),
    tool(
        ItemKind::WoodenShovel,
        "wooden_shovel",
        819,
        ToolKind::Shovel,
        MiningLevel::Wood,
    ),
    tool(
        ItemKind::StoneShovel,
        "stone_shovel",
        824,
        ToolKind::Shovel,
        MiningLevel::Stone,
    ),
    tool(
        ItemKind::IronShovel,
        "iron_shovel",
        834,
        ToolKind::Shovel,
        MiningLevel::Iron,
    ),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn categories_share_place_and_mining_behavior() {
        assert_eq!(
            ItemKind::Stone.definition().placed_block(),
            Some(BlockKind::Stone)
        );
        assert_eq!(ItemKind::WoodenPickaxe.definition().placed_block(), None);
        let stone = BlockKind::Stone.definition();
        let glass = BlockKind::Glass.definition();
        let dirt = BlockKind::Dirt.definition();
        assert!(ItemKind::WoodenPickaxe.definition().can_mine(stone));
        assert!(!ItemKind::WoodenPickaxe.definition().can_mine(glass));
        assert!(ItemKind::StonePickaxe.definition().can_mine(glass));
        assert!(!ItemKind::IronPickaxe.definition().can_mine(dirt));
        assert!(ItemKind::WoodenShovel.definition().can_mine(dirt));
        assert!(!ItemKind::Stone.definition().can_mine(stone));
    }
}
