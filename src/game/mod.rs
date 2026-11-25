//! Data-driven Java 1.21.1 gameplay core. Protocol IDs live at the edge;
//! generated definitions and shared behaviors replace per-item handler branches.
mod data;
mod inventory;
mod items;
mod player;

pub use data::{
    BlockDefinition, BlockStateDefinition, GameRegistry, ItemDefinition, ToolRule, block_by_name,
    block_by_state, blocks, item_by_id, item_by_name, items, registry,
};
pub use inventory::{ClickResult, Inventory, InventoryClick, SLOT_COUNT};
pub use items::{Hand, ItemStack};
pub use player::{PLAYER_EYE_HEIGHT, Player};

pub const AIR_STATE: i32 = 0;
pub const STONE_STATE: i32 = 1;
pub const GRASS_BLOCK_STATE: i32 = 9;
pub const DIRT_STATE: i32 = 10;
pub const BEDROCK_STATE: i32 = 79;
pub const GLASS_STATE: i32 = 519;
