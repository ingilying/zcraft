//! Unified `BlockBehavior` and `ItemBehavior` abstractions (mirroring vanilla
//! `BlockBehaviour.useWithoutItem` and `Item.use` / `Item.finishUsingItem`).
//!
//! New interactive blocks (chests, crafting tables, furnaces, doors, beds, ...)
//! and usable items (foods, buckets, bows, projectiles, ...) implement these
//! traits instead of adding ad-hoc branches inside packet handlers.
use super::{Hand, ItemStack, Player, block_by_state, item_by_id, registry};
use crate::world::{BlockPos, World};

/// Result of right-clicking a block (`BlockBehavior::on_use`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockUseOutcome {
    /// Block has no right-click interaction; fall through to block placement / item use.
    Pass,
    OpenChest(BlockPos),
    OpenCraftingTable,
    OpenFurnace(BlockPos),
    /// Toggle/update one or more block states (e.g., doors, trapdoors, fence gates).
    SetBlocks(Vec<(BlockPos, i32)>),
    /// Right-click a bed: set respawn point and skip night if nighttime/thunderstorm.
    UseBed(BlockPos),
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

    /// Whether this block falls under gravity when unsupported below (sand, gravel, anvils, concrete powder).
    fn falls_with_gravity(&self) -> bool {
        false
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

struct ToggleOpenBlock;
impl BlockBehavior for ToggleOpenBlock {
    fn on_use(&self, world: &World, pos: BlockPos, state: i32) -> BlockUseOutcome {
        let Some(next) = toggle_open_property(state) else {
            return BlockUseOutcome::Pass;
        };
        let mut updates = vec![(pos, next)];
        if let Some(other) = super::paired_block_pos(pos, state)
            && other.valid()
            && let Some(other_next) = toggle_open_property(world.block_at(other))
        {
            updates.push((other, other_next));
        }
        BlockUseOutcome::SetBlocks(updates)
    }
}

struct BedBlock;
impl BlockBehavior for BedBlock {
    fn on_use(&self, _world: &World, pos: BlockPos, _state: i32) -> BlockUseOutcome {
        BlockUseOutcome::UseBed(pos)
    }
}

struct FallingBlock;
impl BlockBehavior for FallingBlock {
    fn falls_with_gravity(&self) -> bool {
        true
    }
}

fn toggle_open_property(state: i32) -> Option<i32> {
    let reg = registry();
    let (_, def, sdef) = reg.state_by_id(state)?;
    let current = sdef.properties.get("open")?.as_str();
    let next_val = if current == "true" { "false" } else { "true" };
    let mut props: std::collections::BTreeMap<String, String> = sdef
        .properties
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    props.insert("open".into(), next_val.into());
    reg.resolve_state(&def.name, &props)
}

static DEFAULT_BLOCK: DefaultBlock = DefaultBlock;
static CRAFTING_TABLE_BLOCK: CraftingTableBlock = CraftingTableBlock;
static CHEST_BLOCK: ChestBlock = ChestBlock;
static FURNACE_BLOCK: FurnaceBlock = FurnaceBlock;
static TOGGLE_OPEN_BLOCK: ToggleOpenBlock = ToggleOpenBlock;
static BED_BLOCK: BedBlock = BedBlock;
static FALLING_BLOCK: FallingBlock = FallingBlock;

/// Resolve the unified `BlockBehavior` for a block state ID.
pub fn block_behavior(state: i32) -> &'static dyn BlockBehavior {
    let Some((_, def, _)) = registry().state_by_id(state) else {
        return &DEFAULT_BLOCK;
    };
    let path = def.name.strip_prefix("minecraft:").unwrap_or(&def.name);
    match path {
        "crafting_table" => &CRAFTING_TABLE_BLOCK,
        "chest" | "trapped_chest" => &CHEST_BLOCK,
        "furnace" => &FURNACE_BLOCK,
        "iron_door" | "iron_trapdoor" => &DEFAULT_BLOCK,
        "sand" | "red_sand" | "gravel" | "suspicious_sand" | "suspicious_gravel" | "anvil"
        | "chipped_anvil" | "damaged_anvil" | "dragon_egg" => &FALLING_BLOCK,
        _ if path.ends_with("_concrete_powder") => &FALLING_BLOCK,
        _ if path.ends_with("_door")
            || path.ends_with("_trapdoor")
            || path.ends_with("_fence_gate") =>
        {
            &TOGGLE_OPEN_BLOCK
        }
        _ if path.ends_with("_bed") => &BED_BLOCK,
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
    /// Bucket picked up or placed a fluid (block update + survival item swap).
    UseBucket {
        hand: Hand,
        pos: BlockPos,
        new_state: i32,
        result_item: i32,
    },
}

/// Polymorphic behavior for a Minecraft item type.
pub trait ItemBehavior: Send + Sync {
    /// Handle right-clicking with this item (`Item.use`).
    fn on_use(
        &self,
        world: &World,
        player: &Player,
        hand: Hand,
        stack: ItemStack,
    ) -> ItemUseOutcome {
        let _ = (world, player, hand, stack);
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
    fn on_use(
        &self,
        _world: &World,
        player: &Player,
        hand: Hand,
        stack: ItemStack,
    ) -> ItemUseOutcome {
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

struct BucketItem;
impl ItemBehavior for BucketItem {
    fn on_use(
        &self,
        world: &World,
        player: &Player,
        hand: Hand,
        stack: ItemStack,
    ) -> ItemUseOutcome {
        if !player.can_interact() || player.dead {
            return ItemUseOutcome::Pass;
        }
        let Some(def) = item_by_id(stack.item) else {
            return ItemUseOutcome::Pass;
        };
        let reach = if player.is_survival() { 5.0 } else { 5.5 };
        match def.name.as_str() {
            "minecraft:bucket" => {
                let Some((pos, _)) = raycast_block(
                    world,
                    player.eye_position(),
                    player.yaw,
                    player.pitch,
                    reach,
                    true,
                ) else {
                    return ItemUseOutcome::Pass;
                };
                let state = world.block_at(pos);
                let Some((_, bdef, sdef)) = registry().state_by_id(state) else {
                    return ItemUseOutcome::Pass;
                };
                if bdef.name == "minecraft:water"
                    && sdef.properties.get("level").map(String::as_str) == Some("0")
                    && let Some(wb) = super::item_by_name("water_bucket")
                {
                    return ItemUseOutcome::UseBucket {
                        hand,
                        pos,
                        new_state: super::AIR_STATE,
                        result_item: wb.id,
                    };
                }
                if bdef.name == "minecraft:lava"
                    && sdef.properties.get("level").map(String::as_str) == Some("0")
                    && let Some(lb) = super::item_by_name("lava_bucket")
                {
                    return ItemUseOutcome::UseBucket {
                        hand,
                        pos,
                        new_state: super::AIR_STATE,
                        result_item: lb.id,
                    };
                }
                if sdef.properties.get("waterlogged").map(String::as_str) == Some("true")
                    && let Some(unlogged) = set_waterlogged_property(state, false)
                    && let Some(wb) = super::item_by_name("water_bucket")
                {
                    return ItemUseOutcome::UseBucket {
                        hand,
                        pos,
                        new_state: unlogged,
                        result_item: wb.id,
                    };
                }
                ItemUseOutcome::Pass
            }
            "minecraft:water_bucket" => {
                let Some((hit_pos, normal)) = raycast_block(
                    world,
                    player.eye_position(),
                    player.yaw,
                    player.pitch,
                    reach,
                    false,
                ) else {
                    return ItemUseOutcome::Pass;
                };
                let Some(empty) = super::item_by_name("bucket") else {
                    return ItemUseOutcome::Pass;
                };
                let hit_state = world.block_at(hit_pos);
                if let Some((_, _, sdef)) = registry().state_by_id(hit_state)
                    && sdef.properties.get("waterlogged").map(String::as_str) == Some("false")
                    && let Some(logged) = set_waterlogged_property(hit_state, true)
                {
                    return ItemUseOutcome::UseBucket {
                        hand,
                        pos: hit_pos,
                        new_state: logged,
                        result_item: empty.id,
                    };
                }
                let target = BlockPos([
                    hit_pos.0[0] + normal[0],
                    hit_pos.0[1] + normal[1],
                    hit_pos.0[2] + normal[2],
                ]);
                if target.valid()
                    && block_by_state(world.block_at(target))
                        .is_some_and(|d| d.air || d.replaceable)
                    && let Some(water) = super::block_by_name("water")
                {
                    return ItemUseOutcome::UseBucket {
                        hand,
                        pos: target,
                        new_state: water.default_state,
                        result_item: empty.id,
                    };
                }
                ItemUseOutcome::Pass
            }
            "minecraft:lava_bucket" => {
                let Some((hit_pos, normal)) = raycast_block(
                    world,
                    player.eye_position(),
                    player.yaw,
                    player.pitch,
                    reach,
                    false,
                ) else {
                    return ItemUseOutcome::Pass;
                };
                let Some(empty) = super::item_by_name("bucket") else {
                    return ItemUseOutcome::Pass;
                };
                let target = BlockPos([
                    hit_pos.0[0] + normal[0],
                    hit_pos.0[1] + normal[1],
                    hit_pos.0[2] + normal[2],
                ]);
                if target.valid()
                    && block_by_state(world.block_at(target))
                        .is_some_and(|d| d.air || d.replaceable)
                    && let Some(lava) = super::block_by_name("lava")
                {
                    return ItemUseOutcome::UseBucket {
                        hand,
                        pos: target,
                        new_state: lava.default_state,
                        result_item: empty.id,
                    };
                }
                ItemUseOutcome::Pass
            }
            _ => ItemUseOutcome::Pass,
        }
    }
}

fn set_waterlogged_property(state: i32, waterlogged: bool) -> Option<i32> {
    let reg = registry();
    let (_, def, sdef) = reg.state_by_id(state)?;
    if !sdef.properties.contains_key("waterlogged") {
        return None;
    }
    let mut props: std::collections::BTreeMap<String, String> = sdef
        .properties
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    props.insert(
        "waterlogged".into(),
        if waterlogged { "true" } else { "false" }.into(),
    );
    reg.resolve_state(&def.name, &props)
}

/// 3D voxel DDA raycast from `eye` along `(yaw, pitch)` up to `max_dist`.
/// Returns `(hit_block_pos, hit_face_normal)`.
fn raycast_block(
    world: &World,
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    max_dist: f64,
    stop_on_fluid_source: bool,
) -> Option<(BlockPos, [i32; 3])> {
    let yaw_rad = f64::from(yaw).to_radians();
    let pitch_rad = f64::from(pitch).to_radians();
    let dx = -yaw_rad.sin() * pitch_rad.cos();
    let dy = -pitch_rad.sin();
    let dz = yaw_rad.cos() * pitch_rad.cos();

    let mut x = eye[0].floor() as i32;
    let mut y = eye[1].floor() as i32;
    let mut z = eye[2].floor() as i32;

    let step_x = if dx >= 0.0 { 1 } else { -1 };
    let step_y = if dy >= 0.0 { 1 } else { -1 };
    let step_z = if dz >= 0.0 { 1 } else { -1 };

    let t_delta_x = if dx.abs() < 1e-9 {
        f64::INFINITY
    } else {
        1.0 / dx.abs()
    };
    let t_delta_y = if dy.abs() < 1e-9 {
        f64::INFINITY
    } else {
        1.0 / dy.abs()
    };
    let t_delta_z = if dz.abs() < 1e-9 {
        f64::INFINITY
    } else {
        1.0 / dz.abs()
    };

    let mut t_max_x = if dx > 1e-9 {
        (f64::from(x + 1) - eye[0]) * t_delta_x
    } else if dx < -1e-9 {
        (eye[0] - f64::from(x)) * t_delta_x
    } else {
        f64::INFINITY
    };
    let mut t_max_y = if dy > 1e-9 {
        (f64::from(y + 1) - eye[1]) * t_delta_y
    } else if dy < -1e-9 {
        (eye[1] - f64::from(y)) * t_delta_y
    } else {
        f64::INFINITY
    };
    let mut t_max_z = if dz > 1e-9 {
        (f64::from(z + 1) - eye[2]) * t_delta_z
    } else if dz < -1e-9 {
        (eye[2] - f64::from(z)) * t_delta_z
    } else {
        f64::INFINITY
    };

    let mut normal = [0, 1, 0];
    let mut t = 0.0;
    while t <= max_dist {
        let pos = BlockPos([x, y, z]);
        if pos.valid() {
            let state = world.block_at(pos);
            if let Some((_, bdef, sdef)) = registry().state_by_id(state) {
                if stop_on_fluid_source {
                    let is_source = (bdef.name == "minecraft:water"
                        || bdef.name == "minecraft:lava")
                        && sdef.properties.get("level").map(String::as_str) == Some("0");
                    let is_waterlogged =
                        sdef.properties.get("waterlogged").map(String::as_str) == Some("true");
                    if is_source || is_waterlogged || sdef.collision {
                        return Some((pos, normal));
                    }
                } else if !sdef.air && !sdef.liquid {
                    return Some((pos, normal));
                }
            }
        }
        if t_max_x <= t_max_y && t_max_x <= t_max_z {
            t = t_max_x;
            t_max_x += t_delta_x;
            x += step_x;
            normal = [-step_x, 0, 0];
        } else if t_max_y <= t_max_z {
            t = t_max_y;
            t_max_y += t_delta_y;
            y += step_y;
            normal = [0, -step_y, 0];
        } else {
            t = t_max_z;
            t_max_z += t_delta_z;
            z += step_z;
            normal = [0, 0, -step_z];
        }
    }
    None
}

static DEFAULT_ITEM: DefaultItem = DefaultItem;
static FOOD_ITEM: FoodItem = FoodItem;
static BUCKET_ITEM: BucketItem = BucketItem;

/// Resolve the unified `ItemBehavior` for an item ID (data-driven via `ItemComponents`).
pub fn item_behavior(item_id: i32) -> &'static dyn ItemBehavior {
    let Some(def) = item_by_id(item_id) else {
        return &DEFAULT_ITEM;
    };
    if def.components.food.is_some() {
        &FOOD_ITEM
    } else if matches!(
        def.name.as_str(),
        "minecraft:bucket" | "minecraft:water_bucket" | "minecraft:lava_bucket"
    ) {
        &BUCKET_ITEM
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
        let world = World::new();
        let mut player = Player::at_spawn(1, uuid::Uuid::nil(), "Steve".into(), GameMode::Survival);
        player.food = 10;
        player.saturation = 0.0;
        player.health = 14.0;

        let steak = item_by_name("cooked_beef").unwrap().id;
        let stack = ItemStack::new(steak, 1).unwrap();
        assert_eq!(
            item_behavior(steak).on_use(&world, &player, Hand::Main, stack),
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

        // Two-block wooden door toggles open=true on BOTH lower and upper halves.
        let mut lower_p = std::collections::BTreeMap::new();
        lower_p.insert("half".into(), "lower".into());
        lower_p.insert("open".into(), "false".into());
        let mut upper_p = std::collections::BTreeMap::new();
        upper_p.insert("half".into(), "upper".into());
        upper_p.insert("open".into(), "false".into());
        let lower = registry()
            .resolve_state("minecraft:oak_door", &lower_p)
            .unwrap();
        let upper = registry()
            .resolve_state("minecraft:oak_door", &upper_p)
            .unwrap();
        let pos_up = BlockPos([pos.0[0], pos.0[1] + 1, pos.0[2]]);
        world.set_block(pos, lower);
        world.set_block(pos_up, upper);
        let BlockUseOutcome::SetBlocks(updates) = block_behavior(lower).on_use(&world, pos, lower)
        else {
            panic!("expected SetBlocks for oak_door");
        };
        assert_eq!(updates.len(), 2);
        for &(_, st) in &updates {
            assert_eq!(
                registry()
                    .state_by_id(st)
                    .unwrap()
                    .2
                    .properties
                    .get("open")
                    .map(String::as_str),
                Some("true")
            );
        }

        // Iron door cannot be toggled by hand.
        let iron_door = crate::game::block_by_name("iron_door")
            .unwrap()
            .default_state;
        assert_eq!(
            block_behavior(iron_door).on_use(&world, pos, iron_door),
            BlockUseOutcome::Pass
        );

        // Bed returns UseBed(pos).
        let red_bed = crate::game::block_by_name("red_bed").unwrap().default_state;
        assert_eq!(
            block_behavior(red_bed).on_use(&world, pos, red_bed),
            BlockUseOutcome::UseBed(pos)
        );

        // Gravity blocks report falls_with_gravity() == true.
        let sand = crate::game::block_by_name("sand").unwrap().default_state;
        assert!(block_behavior(sand).falls_with_gravity());
        assert!(!block_behavior(table).falls_with_gravity());
    }

    #[test]
    fn bucket_item_places_and_scoops_water_via_raycast() {
        let world = World::new();
        let mut player = Player::at_spawn(1, uuid::Uuid::nil(), "Steve".into(), GameMode::Survival);
        // Stand at (8.5, 64.0, 8.5) looking straight down at grass block (8, 63, 8).
        player.position = [8.5, 64.0, 8.5];
        player.yaw = 0.0;
        player.pitch = 90.0;

        let water_bucket = item_by_name("water_bucket").unwrap().id;
        let bucket = item_by_name("bucket").unwrap().id;
        let water_source = crate::game::block_by_name("water").unwrap().default_state;

        let place = item_behavior(water_bucket).on_use(
            &world,
            &player,
            Hand::Main,
            ItemStack::new(water_bucket, 1).unwrap(),
        );
        assert_eq!(
            place,
            ItemUseOutcome::UseBucket {
                hand: Hand::Main,
                pos: BlockPos([8, 64, 8]),
                new_state: water_source,
                result_item: bucket,
            }
        );
        world.set_block(BlockPos([8, 64, 8]), water_source);

        // Empty bucket scoops the water source back up.
        let scoop = item_behavior(bucket).on_use(
            &world,
            &player,
            Hand::Main,
            ItemStack::new(bucket, 1).unwrap(),
        );
        assert_eq!(
            scoop,
            ItemUseOutcome::UseBucket {
                hand: Hand::Main,
                pos: BlockPos([8, 64, 8]),
                new_state: crate::game::AIR_STATE,
                result_item: water_bucket,
            }
        );
    }
}
