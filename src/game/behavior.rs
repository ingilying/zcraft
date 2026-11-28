//! Unified `BlockBehavior` and `ItemBehavior` abstractions (mirroring vanilla
//! `BlockBehaviour.useWithoutItem` and `Item.use` / `Item.finishUsingItem`).
//!
//! Behaviors execute domain logic directly against `UseContext` and return a
//! closed set of primitive `GameEffects`. Adding new interactive blocks or
//! usable items composes these effects without modifying network packet handlers.
use super::{Hand, Inventory, Item, ItemStack, Player, block_by_state, registry};
use crate::world::{BlockPos, DAY_LENGTH, World};

/// Protocol-agnostic container screen opened by a block interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerScreen {
    Chest(BlockPos),
    CraftingTable,
    Furnace(BlockPos),
}

/// Mutable domain context passed into `BlockBehavior::on_use` and `ItemBehavior::on_use`.
pub struct UseContext<'a> {
    pub world: &'a World,
    pub player: &'a mut Player,
    pub inventory: &'a mut Inventory,
    pub tick: u64,
}

/// Unified domain side-effects returned by `BlockBehavior::on_use` and `ItemBehavior::on_use`.
///
/// The session layer applies these effects uniformly (broadcasting block updates,
/// opening screens, syncing inventory, sending chat messages) without branching
/// on individual block or item types.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GameEffects {
    /// True if the interaction was consumed (prevents fallthrough to block placement).
    pub handled: bool,
    /// Block state mutations to apply to the world and broadcast to watchers.
    pub block_updates: Vec<(BlockPos, i32)>,
    /// Container screen to open for the acting player.
    pub open_screen: Option<ContainerScreen>,
    /// Private system chat messages for the acting player.
    pub player_messages: Vec<String>,
    /// Broadcast system chat messages for all online players.
    pub broadcast_messages: Vec<String>,
    /// Update world day time (e.g., sleeping through the night).
    pub set_day_time: Option<i64>,
    /// Clear active rain/thunder weather for the given duration in ticks.
    pub clear_weather_ticks: Option<u32>,
    /// Overflow items to drop at the acting player's position.
    pub player_drops: Vec<ItemStack>,
    /// True if the acting player's inventory/equipment changed and should be resynced.
    pub inventory_changed: bool,
}

impl GameEffects {
    pub fn pass() -> Self {
        Self::default()
    }

    pub fn consumed() -> Self {
        Self {
            handled: true,
            ..Self::default()
        }
    }
}

/// Polymorphic behavior for a Minecraft block type.
pub trait BlockBehavior: Send + Sync {
    /// Handle right-clicking this block without sneaking.
    fn on_use(&self, ctx: &mut UseContext<'_>, pos: BlockPos, state: i32) -> GameEffects {
        let _ = (ctx, pos, state);
        GameEffects::pass()
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
    fn on_use(&self, _ctx: &mut UseContext<'_>, _pos: BlockPos, _state: i32) -> GameEffects {
        let mut effects = GameEffects::consumed();
        effects.open_screen = Some(ContainerScreen::CraftingTable);
        effects
    }
}

struct ChestBlock;
impl BlockBehavior for ChestBlock {
    fn on_use(&self, ctx: &mut UseContext<'_>, pos: BlockPos, _state: i32) -> GameEffects {
        let above = BlockPos([pos.0[0], pos.0[1] + 1, pos.0[2]]);
        let blocked = above.valid()
            && block_by_state(ctx.world.block_at(above))
                .is_some_and(|d| d.collision && d.light_block == 15);
        if blocked || ctx.world.chest_items(pos).is_none() {
            GameEffects::pass()
        } else {
            let mut effects = GameEffects::consumed();
            effects.open_screen = Some(ContainerScreen::Chest(pos));
            effects
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
    fn on_use(&self, ctx: &mut UseContext<'_>, pos: BlockPos, _state: i32) -> GameEffects {
        if ctx.world.furnace_data(pos).is_some() {
            let mut effects = GameEffects::consumed();
            effects.open_screen = Some(ContainerScreen::Furnace(pos));
            effects
        } else {
            GameEffects::pass()
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
    fn on_use(&self, ctx: &mut UseContext<'_>, pos: BlockPos, state: i32) -> GameEffects {
        let Some(next) = toggle_open_property(state) else {
            return GameEffects::pass();
        };
        let mut effects = GameEffects::consumed();
        effects.block_updates.push((pos, next));
        if let Some(other) = super::paired_block_pos(pos, state)
            && other.valid()
            && let Some(other_next) = toggle_open_property(ctx.world.block_at(other))
        {
            effects.block_updates.push((other, other_next));
        }
        effects
    }
}

struct BedBlock;
impl BlockBehavior for BedBlock {
    fn on_use(&self, ctx: &mut UseContext<'_>, pos: BlockPos, _state: i32) -> GameEffects {
        let mut effects = GameEffects::consumed();
        if ctx.player.respawn_point != Some(pos) {
            ctx.player.respawn_point = Some(pos);
            effects.player_messages.push("Respawn point set".into());
        }
        let day_time = ctx.world.time().day_time;
        let time_of_day = day_time.rem_euclid(DAY_LENGTH);
        let weather = ctx.world.weather();
        let can_sleep = (12_541..=23_458).contains(&time_of_day) || weather.thundering;
        if can_sleep {
            effects.set_day_time = Some(day_time - time_of_day + DAY_LENGTH);
            if weather.raining || weather.thundering {
                effects.clear_weather_ticks = Some(12_000);
            }
            effects
                .broadcast_messages
                .push("Sleeping through this night".into());
        } else {
            effects
                .player_messages
                .push("You can sleep only at night or during thunderstorms".into());
        }
        effects
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

/// Polymorphic behavior for a Minecraft item type.
pub trait ItemBehavior: Send + Sync {
    /// Handle right-clicking with this item (`Item.use`).
    fn on_use(&self, ctx: &mut UseContext<'_>, hand: Hand, stack: ItemStack) -> GameEffects {
        let _ = (ctx, hand, stack);
        GameEffects::pass()
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
    fn on_use(&self, ctx: &mut UseContext<'_>, hand: Hand, stack: ItemStack) -> GameEffects {
        if !ctx.player.is_survival() || ctx.player.dead {
            return GameEffects::pass();
        }
        let Some(props) = stack.definition().components.food else {
            return GameEffects::pass();
        };
        if props.always_edible || ctx.player.food < 20 {
            ctx.player.eating = Some((hand, stack.item, ctx.tick, props.eat_ticks));
            GameEffects::consumed()
        } else {
            GameEffects::pass()
        }
    }

    fn on_finish_use(&self, player: &mut Player, stack: ItemStack) -> bool {
        if !player.is_survival() || player.dead {
            return false;
        }
        let Some(props) = stack.definition().components.food else {
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
    fn on_use(&self, ctx: &mut UseContext<'_>, hand: Hand, stack: ItemStack) -> GameEffects {
        if !ctx.player.can_interact() || ctx.player.dead {
            return GameEffects::pass();
        }
        let reach = if ctx.player.is_survival() { 5.0 } else { 5.5 };
        let action: Option<(BlockPos, i32, Item)> = match stack.item.name() {
            "minecraft:bucket" => {
                let Some((pos, _)) = raycast_block(
                    ctx.world,
                    ctx.player.eye_position(),
                    ctx.player.yaw,
                    ctx.player.pitch,
                    reach,
                    true,
                ) else {
                    return GameEffects::pass();
                };
                let state = ctx.world.block_at(pos);
                let Some((_, bdef, sdef)) = registry().state_by_id(state) else {
                    return GameEffects::pass();
                };
                if bdef.name == "minecraft:water"
                    && sdef.properties.get("level").map(String::as_str) == Some("0")
                {
                    Item::by_name("water_bucket").map(|wb| (pos, super::AIR_STATE, wb))
                } else if bdef.name == "minecraft:lava"
                    && sdef.properties.get("level").map(String::as_str) == Some("0")
                {
                    Item::by_name("lava_bucket").map(|lb| (pos, super::AIR_STATE, lb))
                } else if sdef.properties.get("waterlogged").map(String::as_str) == Some("true")
                    && let Some(unlogged) = set_waterlogged_property(state, false)
                {
                    Item::by_name("water_bucket").map(|wb| (pos, unlogged, wb))
                } else {
                    None
                }
            }
            "minecraft:water_bucket" => {
                let Some((hit_pos, normal)) = raycast_block(
                    ctx.world,
                    ctx.player.eye_position(),
                    ctx.player.yaw,
                    ctx.player.pitch,
                    reach,
                    false,
                ) else {
                    return GameEffects::pass();
                };
                let Some(empty) = Item::by_name("bucket") else {
                    return GameEffects::pass();
                };
                let hit_state = ctx.world.block_at(hit_pos);
                if let Some((_, _, sdef)) = registry().state_by_id(hit_state)
                    && sdef.properties.get("waterlogged").map(String::as_str) == Some("false")
                    && let Some(logged) = set_waterlogged_property(hit_state, true)
                {
                    Some((hit_pos, logged, empty))
                } else {
                    let target = BlockPos([
                        hit_pos.0[0] + normal[0],
                        hit_pos.0[1] + normal[1],
                        hit_pos.0[2] + normal[2],
                    ]);
                    if target.valid()
                        && block_by_state(ctx.world.block_at(target))
                            .is_some_and(|d| d.air || d.replaceable)
                    {
                        super::block_by_name("water").map(|w| (target, w.default_state, empty))
                    } else {
                        None
                    }
                }
            }
            "minecraft:lava_bucket" => {
                let Some((hit_pos, normal)) = raycast_block(
                    ctx.world,
                    ctx.player.eye_position(),
                    ctx.player.yaw,
                    ctx.player.pitch,
                    reach,
                    false,
                ) else {
                    return GameEffects::pass();
                };
                let Some(empty) = Item::by_name("bucket") else {
                    return GameEffects::pass();
                };
                let target = BlockPos([
                    hit_pos.0[0] + normal[0],
                    hit_pos.0[1] + normal[1],
                    hit_pos.0[2] + normal[2],
                ]);
                if target.valid()
                    && block_by_state(ctx.world.block_at(target))
                        .is_some_and(|d| d.air || d.replaceable)
                {
                    super::block_by_name("lava").map(|l| (target, l.default_state, empty))
                } else {
                    None
                }
            }
            _ => None,
        };

        let Some((pos, new_state, result_item)) = action else {
            return GameEffects::pass();
        };
        if ctx.world.block_at(pos) == new_state {
            return GameEffects::pass();
        }
        let mut effects = GameEffects::consumed();
        effects.block_updates.push((pos, new_state));
        if ctx.player.is_survival()
            && let Some(replacement) = ItemStack::new(result_item, 1)
        {
            if let Some(overflow) = ctx.inventory.replace_hand_one(hand, replacement) {
                effects.player_drops.push(overflow);
            }
            effects.inventory_changed = true;
        }
        effects
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

/// Resolve the unified `ItemBehavior` for an `Item` (data-driven via `ItemComponents`).
pub fn item_behavior(item: Item) -> &'static dyn ItemBehavior {
    if item.definition().components.food.is_some() {
        &FOOD_ITEM
    } else if matches!(
        item.name(),
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
    use crate::config::GameMode;

    #[test]
    fn food_item_behavior_and_hunger_regen_match_vanilla() {
        let world = World::new();
        let mut player = Player::at_spawn(1, uuid::Uuid::nil(), "Steve".into(), GameMode::Survival);
        let mut inv = Inventory::default();
        player.food = 10;
        player.saturation = 0.0;
        player.health = 14.0;

        let steak = Item::by_name("cooked_beef").unwrap();
        let stack = ItemStack::new(steak, 1).unwrap();
        let mut ctx = UseContext {
            world: &world,
            player: &mut player,
            inventory: &mut inv,
            tick: 0,
        };
        let effects = item_behavior(steak).on_use(&mut ctx, Hand::Main, stack);
        assert!(effects.handled);
        assert!(matches!(player.eating, Some((Hand::Main, item, _, 32)) if item == steak));
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
    fn block_behavior_dispatches_crafting_table_chest_furnace_doors_and_bed() {
        let world = World::new();
        let mut player = Player::at_spawn(1, uuid::Uuid::nil(), "Steve".into(), GameMode::Survival);
        let mut inv = Inventory::default();
        let pos = BlockPos([2, 64, 2]);
        let table = crate::game::block_by_name("crafting_table")
            .unwrap()
            .default_state;
        world.set_block(pos, table);
        {
            let mut ctx = UseContext {
                world: &world,
                player: &mut player,
                inventory: &mut inv,
                tick: 0,
            };
            let fx = block_behavior(table).on_use(&mut ctx, pos, table);
            assert_eq!(fx.open_screen, Some(ContainerScreen::CraftingTable));
        }

        let furnace = crate::game::block_by_name("furnace").unwrap().default_state;
        world.set_block(pos, furnace);
        {
            let mut ctx = UseContext {
                world: &world,
                player: &mut player,
                inventory: &mut inv,
                tick: 0,
            };
            let fx = block_behavior(furnace).on_use(&mut ctx, pos, furnace);
            assert_eq!(fx.open_screen, Some(ContainerScreen::Furnace(pos)));
        }

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
        let fx = {
            let mut ctx = UseContext {
                world: &world,
                player: &mut player,
                inventory: &mut inv,
                tick: 0,
            };
            block_behavior(lower).on_use(&mut ctx, pos, lower)
        };
        assert_eq!(fx.block_updates.len(), 2);
        for &(_, st) in &fx.block_updates {
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
        {
            let mut ctx = UseContext {
                world: &world,
                player: &mut player,
                inventory: &mut inv,
                tick: 0,
            };
            assert!(
                !block_behavior(iron_door)
                    .on_use(&mut ctx, pos, iron_door)
                    .handled
            );
        }

        // Bed sets respawn point and emits sleep effect at night.
        let red_bed = crate::game::block_by_name("red_bed").unwrap().default_state;
        world.set_day_time(14_000);
        let fx = {
            let mut ctx = UseContext {
                world: &world,
                player: &mut player,
                inventory: &mut inv,
                tick: 0,
            };
            block_behavior(red_bed).on_use(&mut ctx, pos, red_bed)
        };
        assert!(fx.handled);
        assert_eq!(player.respawn_point, Some(pos));
        assert_eq!(fx.set_day_time, Some(DAY_LENGTH));

        // Gravity blocks report falls_with_gravity() == true.
        let sand = crate::game::block_by_name("sand").unwrap().default_state;
        assert!(block_behavior(sand).falls_with_gravity());
        assert!(!block_behavior(table).falls_with_gravity());
    }

    #[test]
    fn bucket_item_places_and_scoops_water_via_raycast() {
        let world = World::new();
        let mut player = Player::at_spawn(1, uuid::Uuid::nil(), "Steve".into(), GameMode::Survival);
        let mut inv = Inventory::default();
        // Stand at (8.5, 64.0, 8.5) looking straight down at grass block (8, 63, 8).
        player.position = [8.5, 64.0, 8.5];
        player.yaw = 0.0;
        player.pitch = 90.0;

        let water_bucket = Item::by_name("water_bucket").unwrap();
        let bucket = Item::by_name("bucket").unwrap();
        let water_source = crate::game::block_by_name("water").unwrap().default_state;
        inv.set_slot(36, ItemStack::new(water_bucket, 1));

        let place = {
            let mut ctx = UseContext {
                world: &world,
                player: &mut player,
                inventory: &mut inv,
                tick: 0,
            };
            item_behavior(water_bucket).on_use(
                &mut ctx,
                Hand::Main,
                ItemStack::new(water_bucket, 1).unwrap(),
            )
        };
        assert!(place.handled);
        assert_eq!(
            place.block_updates,
            vec![(BlockPos([8, 64, 8]), water_source)]
        );
        assert!(place.inventory_changed);
        assert_eq!(inv.hand(Hand::Main), ItemStack::new(bucket, 1));
        world.set_block(BlockPos([8, 64, 8]), water_source);

        // Empty bucket scoops the water source back up.
        let scoop = {
            let mut ctx = UseContext {
                world: &world,
                player: &mut player,
                inventory: &mut inv,
                tick: 0,
            };
            item_behavior(bucket).on_use(&mut ctx, Hand::Main, ItemStack::new(bucket, 1).unwrap())
        };
        assert!(scoop.handled);
        assert_eq!(
            scoop.block_updates,
            vec![(BlockPos([8, 64, 8]), crate::game::AIR_STATE)]
        );
        assert_eq!(inv.hand(Hand::Main), ItemStack::new(water_bucket, 1));
    }
}
