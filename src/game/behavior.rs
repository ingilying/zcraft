//! Unified `BlockBehavior` and `ItemBehavior` abstractions (mirroring vanilla
//! `BlockBehaviour.useWithoutItem` and `Item.use` / `Item.finishUsingItem`).
//!
//! New interactive blocks (chests, crafting tables, furnaces, doors, beds, ...)
//! and usable items (foods, buckets, bows, projectiles, ...) implement these
//! traits instead of adding ad-hoc branches inside packet handlers.
use super::{Hand, ItemStack, Player, block_by_state, item_by_id, registry};
use crate::world::{BlockPos, World};

/// Result of right-clicking a block (`BlockBehavior::on_use`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockUseOutcome {
    /// Block has no right-click interaction; fall through to block placement / item use.
    Pass,
    OpenChest(BlockPos),
    OpenCraftingTable,
    OpenFurnace(BlockPos),
}

/// Polymorphic behavior for a Minecraft block type.
pub trait BlockBehavior: Send + Sync {
    /// Handle right-clicking this block without sneaking.
    fn on_use(&self, world: &World, pos: BlockPos, state: i32) -> BlockUseOutcome {
        let _ = (world, pos, state);
        BlockUseOutcome::Pass
    }

    /// Extra items to drop from block-entity storage when this block is broken.
    fn container_drops(&self, world: &World, pos: BlockPos) -> Vec<ItemStack> {
        let _ = (world, pos);
        Vec::new()
    }
}

struct DefaultBlock;
impl BlockBehavior for DefaultBlock {}

struct CraftingTableBlock;
impl BlockBehavior for CraftingTableBlock {
    fn on_use(&self, _world: &World, _pos: BlockPos, _state: i32) -> BlockUseOutcome {
        BlockUseOutcome::OpenCraftingTable
    }
}

struct ChestBlock;
impl BlockBehavior for ChestBlock {
    fn on_use(&self, world: &World, pos: BlockPos, _state: i32) -> BlockUseOutcome {
        let above = BlockPos([pos.0[0], pos.0[1] + 1, pos.0[2]]);
        let blocked = above.valid()
            && block_by_state(world.block_at(above))
                .is_some_and(|d| d.collision && d.light_block == 15);
        if blocked || world.chest_items(pos).is_none() {
            BlockUseOutcome::Pass
        } else {
            BlockUseOutcome::OpenChest(pos)
        }
    }

    fn container_drops(&self, world: &World, pos: BlockPos) -> Vec<ItemStack> {
        world
            .chest_items(pos)
            .map(|items| items.into_iter().flatten().collect())
            .unwrap_or_default()
    }
}

struct FurnaceBlock;
impl BlockBehavior for FurnaceBlock {
    fn on_use(&self, world: &World, pos: BlockPos, _state: i32) -> BlockUseOutcome {
        if world.furnace_data(pos).is_some() {
            BlockUseOutcome::OpenFurnace(pos)
        } else {
            BlockUseOutcome::Pass
        }
    }

    fn container_drops(&self, world: &World, pos: BlockPos) -> Vec<ItemStack> {
        world
            .furnace_data(pos)
            .map(|f| f.slots().into_iter().flatten().collect())
            .unwrap_or_default()
    }
}

static DEFAULT_BLOCK: DefaultBlock = DefaultBlock;
static CRAFTING_TABLE_BLOCK: CraftingTableBlock = CraftingTableBlock;
static CHEST_BLOCK: ChestBlock = ChestBlock;
static FURNACE_BLOCK: FurnaceBlock = FurnaceBlock;

/// Resolve the unified `BlockBehavior` for a block state ID.
pub fn block_behavior(state: i32) -> &'static dyn BlockBehavior {
    let Some((_, def, _)) = registry().state_by_id(state) else {
        return &DEFAULT_BLOCK;
    };
    match def.name.as_str() {
        "minecraft:crafting_table" => &CRAFTING_TABLE_BLOCK,
        "minecraft:chest" | "minecraft:trapped_chest" => &CHEST_BLOCK,
        "minecraft:furnace" => &FURNACE_BLOCK,
        _ => &DEFAULT_BLOCK,
    }
}

/// Result of right-clicking with an item (`ItemBehavior::on_use`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemUseOutcome {
    Pass,
    StartEating {
        hand: Hand,
        item_id: i32,
        eat_ticks: u16,
    },
}

/// Polymorphic behavior for a Minecraft item type.
pub trait ItemBehavior: Send + Sync {
    /// Handle right-clicking with this item (`Item.use`).
    fn on_use(&self, player: &Player, hand: Hand, stack: ItemStack) -> ItemUseOutcome {
        let _ = (player, hand, stack);
        ItemUseOutcome::Pass
    }

    /// Apply effect when continuous use completes (`Item.finishUsingItem`).
    /// Returns `true` if the item was consumed and player state changed.
    fn on_finish_use(&self, player: &mut Player, stack: ItemStack) -> bool {
        let _ = (player, stack);
        false
    }
}

struct DefaultItem;
impl ItemBehavior for DefaultItem {}

struct FoodItem;
impl ItemBehavior for FoodItem {
    fn on_use(&self, player: &Player, hand: Hand, stack: ItemStack) -> ItemUseOutcome {
        if !player.is_survival() || player.dead {
            return ItemUseOutcome::Pass;
        }
        let Some(props) = item_by_id(stack.item).and_then(|d| d.components.food) else {
            return ItemUseOutcome::Pass;
        };
        if props.always_edible || player.food < 20 {
            ItemUseOutcome::StartEating {
                hand,
                item_id: stack.item,
                eat_ticks: props.eat_ticks,
            }
        } else {
            ItemUseOutcome::Pass
        }
    }

    fn on_finish_use(&self, player: &mut Player, stack: ItemStack) -> bool {
        if !player.is_survival() || player.dead {
            return false;
        }
        let Some(props) = item_by_id(stack.item).and_then(|d| d.components.food) else {
            return false;
        };
        if !props.always_edible && player.food >= 20 {
            return false;
        }
        player.eat(props);
        true
    }
}

static DEFAULT_ITEM: DefaultItem = DefaultItem;
static FOOD_ITEM: FoodItem = FoodItem;

/// Resolve the unified `ItemBehavior` for an item ID (data-driven via `ItemComponents`).
pub fn item_behavior(item_id: i32) -> &'static dyn ItemBehavior {
    if item_by_id(item_id).is_some_and(|d| d.components.food.is_some()) {
        &FOOD_ITEM
    } else {
        &DEFAULT_ITEM
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::GameMode, game::item_by_name};

    #[test]
    fn food_item_behavior_and_hunger_regen_match_vanilla() {
        let mut player = Player::at_spawn(1, uuid::Uuid::nil(), "Steve".into(), GameMode::Survival);
        player.food = 10;
        player.saturation = 0.0;
        player.health = 14.0;

        let steak = item_by_name("cooked_beef").unwrap().id;
        let stack = ItemStack::new(steak, 1).unwrap();
        assert_eq!(
            item_behavior(steak).on_use(&player, Hand::Main, stack),
            ItemUseOutcome::StartEating {
                hand: Hand::Main,
                item_id: steak,
                eat_ticks: 32,
            }
        );
        assert!(item_behavior(steak).on_finish_use(&mut player, stack));
        assert_eq!(player.food, 18);
        assert!((player.saturation - 12.8).abs() < 0.01);

        // Slow natural regen at food >= 18 heals +1 HP after 80 ticks.
        let (changed, starved) = player.tick_food(80);
        assert!(changed && !starved);
        assert_eq!(player.health, 15.0);

        // Starvation at food == 0 hurts down to 1.0 HP on Normal difficulty.
        player.food = 0;
        player.saturation = 0.0;
        player.health = 2.0;
        let (changed, starved) = player.tick_food(80);
        assert!(changed && starved);
        assert_eq!(player.health, 1.0);
    }

    #[test]
    fn block_behavior_dispatches_crafting_table_chest_and_furnace() {
        let world = World::new();
        let pos = BlockPos([2, 64, 2]);
        let table = crate::game::block_by_name("crafting_table")
            .unwrap()
            .default_state;
        world.set_block(pos, table);
        assert_eq!(
            block_behavior(table).on_use(&world, pos, table),
            BlockUseOutcome::OpenCraftingTable
        );

        let furnace = crate::game::block_by_name("furnace").unwrap().default_state;
        world.set_block(pos, furnace);
        assert_eq!(
            block_behavior(furnace).on_use(&world, pos, furnace),
            BlockUseOutcome::OpenFurnace(pos)
        );
    }
}
