//! Small data-driven gameplay core. Protocol IDs live at the edge; handlers use
//! stable typed definitions so adding blocks and items does not add packet logic.
mod blocks;
mod inventory;
mod items;

pub use blocks::{BlockDefinition, BlockKind, block_by_name, block_by_state, blocks};
pub use inventory::{ClickResult, Inventory, InventoryClick, SLOT_COUNT};
pub use items::{
    Hand, ItemDefinition, ItemKind, ItemStack, ItemUse, item_by_id, item_by_name, items,
};
