//! Data-driven Minecraft Java 1.21.1 crafting & smelting recipe engine.
//!
//! All official `data/minecraft/recipe/*.json` and `data/minecraft/tags/item/*.json`
//! files are extracted from the pinned official 1.21.1 `server.jar` at build time
//! and zlib-compressed into `OUT_DIR/recipes.json.zlib`.
use super::{Item, ItemStack};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    sync::LazyLock,
};

static RECIPES: LazyLock<RecipeRegistry> = LazyLock::new(RecipeRegistry::load);

#[derive(Debug, Deserialize)]
struct RawBundle {
    recipes: HashMap<String, serde_json::Value>,
    item_tags: HashMap<String, RawTag>,
}

#[derive(Debug, Deserialize)]
struct RawTag {
    #[serde(default)]
    values: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
enum Ingredient {
    Item(Item),
    AnyOf(Vec<Item>),
}

impl Ingredient {
    fn matches(&self, item: Item) -> bool {
        match self {
            Self::Item(expected) => *expected == item,
            Self::AnyOf(items) => items.contains(&item),
        }
    }
}

#[derive(Debug)]
struct ShapedRecipe {
    width: usize,
    height: usize,
    pattern: Vec<Option<Ingredient>>,
    result: ItemStack,
}

#[derive(Debug)]
struct ShapelessRecipe {
    ingredients: Vec<Ingredient>,
    result: ItemStack,
}

#[derive(Debug)]
struct SmeltingRecipe {
    ingredient: Ingredient,
    result: ItemStack,
    cooking_time: u16,
}

struct RecipeRegistry {
    shaped: Vec<ShapedRecipe>,
    shapeless: Vec<ShapelessRecipe>,
    smelting: Vec<SmeltingRecipe>,
}

impl RecipeRegistry {
    fn load() -> Self {
        const COMPRESSED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/recipes.json.zlib"));
        let mut decoder = flate2::read::ZlibDecoder::new(COMPRESSED);
        let mut json = Vec::new();
        decoder
            .read_to_end(&mut json)
            .expect("compressed recipes must be valid");
        let bundle: RawBundle =
            serde_json::from_slice(&json).expect("official recipes JSON must parse");

        // Resolve all item tags recursively.
        let mut item_tags: HashMap<String, HashSet<Item>> = HashMap::new();
        for name in bundle.item_tags.keys() {
            let mut visiting = HashSet::new();
            resolve_tag(name, &bundle.item_tags, &mut item_tags, &mut visiting);
        }

        let mut shaped = Vec::new();
        let mut shapeless = Vec::new();
        let mut smelting = Vec::new();

        for value in bundle.recipes.values() {
            let Some(kind) = value.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            match kind {
                "minecraft:crafting_shaped" => {
                    if let Some(r) = parse_shaped(value, &item_tags) {
                        shaped.push(r);
                    }
                }
                "minecraft:crafting_shapeless" => {
                    if let Some(r) = parse_shapeless(value, &item_tags) {
                        shapeless.push(r);
                    }
                }
                "minecraft:smelting" => {
                    if let Some(r) = parse_smelting(value, &item_tags) {
                        smelting.push(r);
                    }
                }
                _ => {}
            }
        }

        Self {
            shaped,
            shapeless,
            smelting,
        }
    }
}

fn resolve_tag(
    name: &str,
    raw: &HashMap<String, RawTag>,
    resolved: &mut HashMap<String, HashSet<Item>>,
    visiting: &mut HashSet<String>,
) -> HashSet<Item> {
    if let Some(existing) = resolved.get(name) {
        return existing.clone();
    }
    if !visiting.insert(name.to_string()) {
        return HashSet::new();
    }
    let mut set = HashSet::new();
    if let Some(tag) = raw.get(name) {
        for entry in &tag.values {
            let entry_str = match entry {
                serde_json::Value::String(s) => Some(s.as_str()),
                serde_json::Value::Object(obj) => obj.get("id").and_then(|v| v.as_str()),
                _ => None,
            };
            let Some(s) = entry_str else {
                continue;
            };
            if let Some(sub_tag) = s.strip_prefix('#') {
                set.extend(resolve_tag(sub_tag, raw, resolved, visiting));
            } else if let Some(item) = Item::by_name(s) {
                set.insert(item);
            }
        }
    }
    visiting.remove(name);
    resolved.insert(name.to_string(), set.clone());
    set
}

fn parse_ingredient(
    val: &serde_json::Value,
    tags: &HashMap<String, HashSet<Item>>,
) -> Option<Ingredient> {
    let mut items = Vec::new();
    collect_ingredient_items(val, tags, &mut items);
    items.sort_unstable_by_key(|item| item.name());
    items.dedup();
    match items.len() {
        0 => None,
        1 => Some(Ingredient::Item(items[0])),
        _ => Some(Ingredient::AnyOf(items)),
    }
}

fn collect_ingredient_items(
    val: &serde_json::Value,
    tags: &HashMap<String, HashSet<Item>>,
    out: &mut Vec<Item>,
) {
    match val {
        serde_json::Value::Array(arr) => {
            for elem in arr {
                collect_ingredient_items(elem, tags, out);
            }
        }
        serde_json::Value::Object(obj) => {
            if let Some(name) = obj.get("item").and_then(|v| v.as_str())
                && let Some(item) = Item::by_name(name)
            {
                out.push(item);
            }
            if let Some(tag) = obj.get("tag").and_then(|v| v.as_str())
                && let Some(set) = tags.get(tag)
            {
                out.extend(set.iter().copied());
            }
        }
        _ => {}
    }
}

fn parse_result(val: &serde_json::Value) -> Option<ItemStack> {
    let obj = val.as_object()?;
    let id_str = obj.get("id")?.as_str()?;
    let count = obj.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as u8;
    let item = Item::by_name(id_str)?;
    ItemStack::new(item, count)
}

fn parse_shaped(
    val: &serde_json::Value,
    tags: &HashMap<String, HashSet<Item>>,
) -> Option<ShapedRecipe> {
    let pattern_rows = val.get("pattern")?.as_array()?;
    if pattern_rows.is_empty() {
        return None;
    }
    let key_map = val.get("key")?.as_object()?;
    let mut keys: HashMap<char, Ingredient> = HashMap::new();
    for (k, v) in key_map {
        let ch = k.chars().next()?;
        if let Some(ing) = parse_ingredient(v, tags) {
            keys.insert(ch, ing);
        }
    }
    let rows: Vec<Vec<char>> = pattern_rows
        .iter()
        .filter_map(|r| r.as_str().map(|s| s.chars().collect()))
        .collect();
    let raw_h = rows.len();
    let raw_w = rows.iter().map(Vec::len).max().unwrap_or(0);
    if raw_w == 0 || raw_h == 0 {
        return None;
    }

    // Trim empty margins just like vanilla ShapedRecipePattern.
    let mut min_x = raw_w;
    let mut max_x = 0usize;
    let mut min_y = raw_h;
    let mut max_y = 0usize;
    let mut any = false;
    for (y, row) in rows.iter().enumerate() {
        for (x, &ch) in row.iter().enumerate() {
            if ch != ' ' {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                any = true;
            }
        }
    }
    if !any {
        return None;
    }
    let width = max_x - min_x + 1;
    let height = max_y - min_y + 1;
    let mut pattern = Vec::with_capacity(width * height);
    for row in &rows[min_y..=max_y] {
        for x in min_x..=max_x {
            let ch = row.get(x).copied().unwrap_or(' ');
            if ch == ' ' {
                pattern.push(None);
            } else {
                pattern.push(Some(keys.get(&ch)?.clone()));
            }
        }
    }
    let result = parse_result(val.get("result")?)?;
    Some(ShapedRecipe {
        width,
        height,
        pattern,
        result,
    })
}

fn parse_shapeless(
    val: &serde_json::Value,
    tags: &HashMap<String, HashSet<Item>>,
) -> Option<ShapelessRecipe> {
    let raw_ings = val.get("ingredients")?.as_array()?;
    let mut ingredients = Vec::with_capacity(raw_ings.len());
    for ing in raw_ings {
        ingredients.push(parse_ingredient(ing, tags)?);
    }
    if ingredients.is_empty() {
        return None;
    }
    let result = parse_result(val.get("result")?)?;
    Some(ShapelessRecipe {
        ingredients,
        result,
    })
}

fn parse_smelting(
    val: &serde_json::Value,
    tags: &HashMap<String, HashSet<Item>>,
) -> Option<SmeltingRecipe> {
    let ingredient = parse_ingredient(val.get("ingredient")?, tags)?;
    let result = parse_result(val.get("result")?)?;
    let cooking_time = val
        .get("cookingtime")
        .and_then(|v| v.as_u64())
        .unwrap_or(200)
        .clamp(1, u64::from(u16::MAX)) as u16;
    Some(SmeltingRecipe {
        ingredient,
        result,
        cooking_time,
    })
}

/// Match a `width x height` crafting grid (2x2 for inventory, 3x3 for crafting table)
/// against all official 1.21.1 shaped and shapeless recipes.
pub fn match_crafting(
    width: usize,
    height: usize,
    grid: &[Option<ItemStack>],
) -> Option<ItemStack> {
    debug_assert_eq!(grid.len(), width * height);
    let reg = &*RECIPES;

    // Compute tight bounding box of non-empty items in the grid.
    let mut min_x = width;
    let mut max_x = 0usize;
    let mut min_y = height;
    let mut max_y = 0usize;
    let mut items = Vec::new();
    for y in 0..height {
        for x in 0..width {
            if let Some(stack) = grid[y * width + x] {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                items.push(stack.item);
            }
        }
    }
    if items.is_empty() {
        return None;
    }
    let box_w = max_x - min_x + 1;
    let box_h = max_y - min_y + 1;

    // 1. Check shaped recipes (normal and horizontally mirrored).
    for recipe in &reg.shaped {
        if recipe.width != box_w || recipe.height != box_h {
            continue;
        }
        let matches = |mirror: bool| -> bool {
            for ry in 0..box_h {
                for rx in 0..box_w {
                    let gx = if mirror { max_x - rx } else { min_x + rx };
                    let gy = min_y + ry;
                    let slot = grid[gy * width + gx].map(|s| s.item);
                    match (&recipe.pattern[ry * box_w + rx], slot) {
                        (Some(ing), Some(item)) if ing.matches(item) => {}
                        (None, None) => {}
                        _ => return false,
                    }
                }
            }
            true
        };
        if matches(false) || (box_w > 1 && matches(true)) {
            return Some(recipe.result);
        }
    }

    // 2. Check shapeless recipes with backtracking assignment.
    for recipe in &reg.shapeless {
        if recipe.ingredients.len() != items.len() {
            continue;
        }
        let mut used = vec![false; items.len()];
        if match_shapeless_backtrack(&recipe.ingredients, 0, &items, &mut used) {
            return Some(recipe.result);
        }
    }
    None
}

fn match_shapeless_backtrack(
    ingredients: &[Ingredient],
    idx: usize,
    items: &[Item],
    used: &mut [bool],
) -> bool {
    if idx == ingredients.len() {
        return true;
    }
    for (i, &item) in items.iter().enumerate() {
        if !used[i] && ingredients[idx].matches(item) {
            used[i] = true;
            if match_shapeless_backtrack(ingredients, idx + 1, items, used) {
                return true;
            }
            used[i] = false;
        }
    }
    false
}

/// Match an input stack against all official 1.21.1 `minecraft:smelting` recipes.
/// Returns `(result_stack, cooking_time_ticks)`.
pub fn match_smelting(input: Option<ItemStack>) -> Option<(ItemStack, u16)> {
    let stack = input?;
    let reg = &*RECIPES;
    for recipe in &reg.smelting {
        if recipe.ingredient.matches(stack.item) {
            return Some((recipe.result, recipe.cooking_time));
        }
    }
    None
}

/// Data-driven vanilla 1.21.1 furnace fuel burn duration in ticks (`20 ticks = 1s`),
/// resolved from the furnace fuel table in `game_data.json`.
pub fn fuel_burn_ticks(item: Item) -> Option<u16> {
    super::registry().fuel_ticks(item)
}

/// Data-driven vanilla 1.21.1 crafting/fuel container remainder
/// (e.g., `lava_bucket` -> `bucket`, `mushroom_stew` -> `bowl`).
pub fn container_remainder(item: Item) -> Option<ItemStack> {
    let rem = super::registry().craft_remainder(item)?;
    ItemStack::new(rem, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack(name: &str) -> Option<ItemStack> {
        ItemStack::new(Item::by_name(name).unwrap(), 1)
    }

    #[test]
    fn official_recipes_support_full_survival_progression() {
        let reg = &*RECIPES;
        assert!(reg.shaped.len() >= 600);
        assert!(reg.shapeless.len() >= 250);
        assert!(reg.smelting.len() >= 70);

        // 1. 2x2 shapeless: oak_log -> 4 oak_planks
        let planks = match_crafting(2, 2, &[stack("oak_log"), None, None, None]).unwrap();
        assert_eq!(planks.definition().name, "minecraft:oak_planks");
        assert_eq!(planks.count, 4);

        // 2. 2x2 shaped: 4 planks -> 1 crafting_table
        let p = stack("oak_planks");
        let table = match_crafting(2, 2, &[p, p, p, p]).unwrap();
        assert_eq!(table.definition().name, "minecraft:crafting_table");
        assert_eq!(table.count, 1);

        // 3. 2x2 shaped: 2 vertical planks -> 4 sticks
        let sticks = match_crafting(2, 2, &[p, None, p, None]).unwrap();
        assert_eq!(sticks.definition().name, "minecraft:stick");
        assert_eq!(sticks.count, 4);

        // 4. 3x3 shaped: wooden_pickaxe (3 planks + 2 sticks)
        let s = stack("stick");
        let pick = match_crafting(3, 3, &[p, p, p, None, s, None, None, s, None]).unwrap();
        assert_eq!(pick.definition().name, "minecraft:wooden_pickaxe");

        // 5. 3x3 shaped: furnace (8 cobblestone ring)
        let c = stack("cobblestone");
        let furnace = match_crafting(3, 3, &[c, c, c, c, None, c, c, c, c]).unwrap();
        assert_eq!(furnace.definition().name, "minecraft:furnace");

        // 6. Smelting: raw_iron -> iron_ingot (200 ticks)
        let (ingot, ticks) = match_smelting(stack("raw_iron")).unwrap();
        assert_eq!(ingot.definition().name, "minecraft:iron_ingot");
        assert_eq!(ticks, 200);

        // 7. Fuel burn times
        assert_eq!(fuel_burn_ticks(Item::by_name("coal").unwrap()), Some(1600));
        assert_eq!(
            fuel_burn_ticks(Item::by_name("oak_planks").unwrap()),
            Some(300)
        );
        assert_eq!(
            fuel_burn_ticks(Item::by_name("lava_bucket").unwrap()),
            Some(20_000)
        );
    }
}
