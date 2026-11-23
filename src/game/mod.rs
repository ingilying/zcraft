//! Small data-driven gameplay core. Protocol IDs live at the edge; handlers use
//! stable typed definitions so adding blocks and items does not add packet logic.
mod blocks;
mod inventory;
mod items;

pub use blocks::{
    BlockDefinition, BlockKind, MiningLevel, MiningRequirement, ToolKind, block_by_name,
    block_by_state, blocks,
};
pub use inventory::{ClickResult, Inventory, InventoryClick, SLOT_COUNT};
pub use items::{
    BlockItem, Hand, ItemCategory, ItemDefinition, ItemKind, ItemStack, ToolItem, item_by_id,
    item_by_name, items,
};
