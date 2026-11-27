//! Data-driven Java 1.21.1 gameplay core. Protocol IDs live at the edge;
//! generated definitions and shared behaviors replace per-item handler branches.
mod behavior;
pub mod containers;
mod data;
mod inventory;
mod items;
mod loot;
mod placement;
mod player;
mod recipes;

pub use behavior::{
    BlockBehavior, ContainerScreen, GameEffects, ItemBehavior, UseContext, block_behavior,
    item_behavior,
};
pub use data::{
    BlockDefinition, BlockStateDefinition, FoodProperties, GameRegistry, ItemComponents,
    ItemDefinition, ToolRule, block_by_name, block_by_state, blocks, item_by_id, item_by_name,
    items, registry,
};
pub use inventory::{ClickResult, Inventory, InventoryClick, SLOT_COUNT};
pub use items::{Hand, Item, ItemStack};
pub use loot::{block_drops, block_drops_with_rng};
pub use placement::{
    PlacementContext, PlacementResult, is_solid_support, paired_block_pos, required_support_offset,
    resolve_placement, resolve_stairs_shape,
};
pub use player::{PLAYER_EYE_HEIGHT, Player};
pub use recipes::{container_remainder, fuel_burn_ticks, match_crafting, match_smelting};

/// Data-driven food component from `assets/protocol/767/game_data.json`.
pub fn food_properties(item: Item) -> Option<FoodProperties> {
    item.definition().components.food
}

pub const AIR_STATE: i32 = 0;
pub const STONE_STATE: i32 = 1;
pub const GRASS_BLOCK_STATE: i32 = 9;
pub const DIRT_STATE: i32 = 10;
pub const BEDROCK_STATE: i32 = 79;
pub const GLASS_STATE: i32 = 519;
