//! Data-driven Minecraft Java 1.21.1 block loot table evaluator.
//!
//! All 982 official `data/minecraft/loot_table/blocks/*.json` definitions are
//! extracted from the pinned official 1.21.1 `server.jar` at build time and
//! zlib-compressed into `OUT_DIR/block_loot_tables.json.zlib`.
use super::{BlockStateDefinition, ItemStack, item_by_id, item_by_name, registry};
use rand::Rng;
use serde::Deserialize;
use std::{collections::HashMap, io::Read, sync::LazyLock};

static BLOCK_LOOT_TABLES: LazyLock<HashMap<String, LootTable>> = LazyLock::new(|| {
    const COMPRESSED: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/block_loot_tables.json.zlib"));
    let mut decoder = flate2::read::ZlibDecoder::new(COMPRESSED);
    let mut json = Vec::new();
    decoder
        .read_to_end(&mut json)
        .expect("compressed block loot tables must be valid");
    serde_json::from_slice(&json).expect("official block loot tables JSON must parse")
});

#[derive(Debug, Deserialize)]
struct LootTable {
    #[serde(default)]
    pools: Vec<LootPool>,
}

#[derive(Debug, Deserialize)]
struct LootPool {
    #[serde(default = "default_one")]
    rolls: NumberProvider,
    #[serde(default)]
    conditions: Vec<LootCondition>,
    #[serde(default)]
    entries: Vec<LootEntry>,
    #[serde(default)]
    functions: Vec<LootFunction>,
}

fn default_one() -> NumberProvider {
    NumberProvider::Constant(1.0)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NumberProvider {
    Constant(f64),
    Typed(TypedNumberProvider),
}

#[derive(Debug, Deserialize)]
struct TypedNumberProvider {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    value: f64,
    #[serde(default)]
    min: f64,
    #[serde(default)]
    max: f64,
    #[serde(default)]
    n: f64,
    #[serde(default)]
    p: f64,
}

impl NumberProvider {
    fn sample_int(&self, rng: &mut impl Rng) -> i32 {
        match self {
            Self::Constant(v) => v.round() as i32,
            Self::Typed(t) => match t.kind.as_str() {
                "minecraft:constant" => t.value.round() as i32,
                "minecraft:binomial" => {
                    let mut count = 0i32;
                    let trials = t.n.round().max(0.0) as u32;
                    for _ in 0..trials {
                        if rng.r#gen::<f64>() < t.p {
                            count += 1;
                        }
                    }
                    count
                }
                // "minecraft:uniform" or implicit uniform
                _ => {
                    let lo = t.min.round() as i32;
                    let hi = t.max.round() as i32;
                    if hi <= lo { lo } else { rng.gen_range(lo..=hi) }
                }
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct LootEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "default_weight")]
    weight: u32,
    #[serde(default)]
    conditions: Vec<LootCondition>,
    #[serde(default)]
    functions: Vec<LootFunction>,
    #[serde(default)]
    children: Vec<LootEntry>,
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
struct LootCondition {
    condition: String,
    #[serde(default)]
    block: Option<String>,
    #[serde(default)]
    properties: HashMap<String, String>,
    #[serde(default)]
    predicate: Option<serde_json::Value>,
    #[serde(default)]
    chance: Option<f64>,
    #[serde(default)]
    chances: Vec<f64>,
    #[serde(default)]
    term: Option<Box<LootCondition>>,
    #[serde(default)]
    terms: Vec<LootCondition>,
}

#[derive(Debug, Deserialize)]
struct LootFunction {
    function: String,
    #[serde(default)]
    conditions: Vec<LootCondition>,
    #[serde(default)]
    count: Option<NumberProvider>,
    #[serde(default)]
    add: bool,
    #[serde(default)]
    limit: Option<LimitSpec>,
}

#[derive(Debug, Deserialize)]
struct LimitSpec {
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
}

struct EvalContext<'a> {
    block_name: &'a str,
    state: &'a BlockStateDefinition,
    tool_name: Option<&'a str>,
}

/// Evaluate the official Minecraft 1.21.1 block loot table for breaking a block
/// with `state` using optional held `tool_item`.
pub fn block_drops(state: i32, tool_item: Option<i32>) -> Vec<ItemStack> {
    let mut rng = rand::thread_rng();
    block_drops_with_rng(state, tool_item, &mut rng)
}

pub fn block_drops_with_rng(
    state: i32,
    tool_item: Option<i32>,
    rng: &mut impl Rng,
) -> Vec<ItemStack> {
    let Some((_, block_def, state_def)) = registry().state_by_id(state) else {
        return Vec::new();
    };
    let Some(table) = BLOCK_LOOT_TABLES.get(&block_def.name) else {
        return Vec::new();
    };
    let tool_name = tool_item.and_then(item_by_id).map(|def| def.name.as_str());
    let ctx = EvalContext {
        block_name: &block_def.name,
        state: state_def,
        tool_name,
    };

    let mut drops = Vec::new();
    for pool in &table.pools {
        if !all_conditions(&pool.conditions, &ctx, rng) {
            continue;
        }
        let rolls = pool.rolls.sample_int(rng).max(0);
        for _ in 0..rolls {
            let mut pool_drops = Vec::new();
            eval_pool_entries(&pool.entries, &ctx, rng, &mut pool_drops);
            for (item_name, mut count) in pool_drops {
                apply_functions(&pool.functions, &ctx, rng, &mut count);
                push_item_drops(&item_name, count, &mut drops);
            }
        }
    }
    drops
}

fn eval_pool_entries(
    entries: &[LootEntry],
    ctx: &EvalContext<'_>,
    rng: &mut impl Rng,
    out: &mut Vec<(String, i32)>,
) {
    if entries.is_empty() {
        return;
    }
    if entries.len() == 1 {
        eval_entry(&entries[0], ctx, rng, out);
        return;
    }
    // Weighted random selection among entries whose top-level conditions pass.
    let mut eligible: Vec<(&LootEntry, u32)> = Vec::with_capacity(entries.len());
    let mut total_weight = 0u32;
    for entry in entries {
        if all_conditions(&entry.conditions, ctx, rng) {
            let w = entry.weight.max(1);
            total_weight = total_weight.saturating_add(w);
            eligible.push((entry, w));
        }
    }
    if total_weight == 0 {
        return;
    }
    let mut pick = rng.gen_range(0..total_weight);
    for (entry, w) in eligible {
        if pick < w {
            eval_entry_unchecked(entry, ctx, rng, out);
            return;
        }
        pick -= w;
    }
}

fn eval_entry(
    entry: &LootEntry,
    ctx: &EvalContext<'_>,
    rng: &mut impl Rng,
    out: &mut Vec<(String, i32)>,
) -> bool {
    if !all_conditions(&entry.conditions, ctx, rng) {
        return false;
    }
    eval_entry_unchecked(entry, ctx, rng, out)
}

fn eval_entry_unchecked(
    entry: &LootEntry,
    ctx: &EvalContext<'_>,
    rng: &mut impl Rng,
    out: &mut Vec<(String, i32)>,
) -> bool {
    match entry.kind.as_str() {
        "minecraft:item" => {
            let Some(name) = &entry.name else {
                return false;
            };
            let mut count = 1i32;
            apply_functions(&entry.functions, ctx, rng, &mut count);
            if count > 0 {
                out.push((name.clone(), count));
            }
            true
        }
        "minecraft:alternatives" => {
            for child in &entry.children {
                if eval_entry(child, ctx, rng, out) {
                    return true;
                }
            }
            false
        }
        "minecraft:sequence" => {
            let mut any = false;
            for child in &entry.children {
                if !eval_entry(child, ctx, rng, out) {
                    break;
                }
                any = true;
            }
            any
        }
        "minecraft:group" => {
            let mut any = false;
            for child in &entry.children {
                any |= eval_entry(child, ctx, rng, out);
            }
            any
        }
        // "minecraft:empty" or "minecraft:dynamic" (shulker contents handled separately)
        _ => true,
    }
}

fn all_conditions(conds: &[LootCondition], ctx: &EvalContext<'_>, rng: &mut impl Rng) -> bool {
    conds.iter().all(|c| eval_condition(c, ctx, rng))
}

fn eval_condition(cond: &LootCondition, ctx: &EvalContext<'_>, rng: &mut impl Rng) -> bool {
    match cond.condition.as_str() {
        "minecraft:survives_explosion" => true,
        "minecraft:entity_properties" => true,
        "minecraft:location_check" => true,
        "minecraft:block_state_property" => {
            if let Some(block) = &cond.block
                && block != ctx.block_name
            {
                return false;
            }
            cond.properties
                .iter()
                .all(|(k, v)| ctx.state.properties.get(k).map(String::as_str) == Some(v.as_str()))
        }
        "minecraft:match_tool" => eval_match_tool(cond.predicate.as_ref(), ctx.tool_name),
        "minecraft:random_chance" => {
            let chance = cond.chance.unwrap_or(0.0);
            rng.r#gen::<f64>() < chance
        }
        "minecraft:table_bonus" => {
            // Level 0 (no Fortune enchantment yet).
            let chance = cond.chances.first().copied().unwrap_or(0.0);
            rng.r#gen::<f64>() < chance
        }
        "minecraft:any_of" => cond.terms.iter().any(|t| eval_condition(t, ctx, rng)),
        "minecraft:all_of" => cond.terms.iter().all(|t| eval_condition(t, ctx, rng)),
        "minecraft:inverted" => cond
            .term
            .as_deref()
            .is_none_or(|t| !eval_condition(t, ctx, rng)),
        _ => false,
    }
}

fn eval_match_tool(predicate: Option<&serde_json::Value>, tool_name: Option<&str>) -> bool {
    let Some(pred) = predicate.and_then(|v| v.as_object()) else {
        return true;
    };
    // Enchantment predicates (e.g. minecraft:silk_touch) never match without enchantments.
    if let Some(sub) = pred.get("predicates").and_then(|v| v.as_object())
        && sub.contains_key("minecraft:enchantments")
    {
        return false;
    }
    if let Some(items) = pred.get("items") {
        let Some(held) = tool_name else {
            return false;
        };
        let matched = match items {
            serde_json::Value::String(s) => s == held,
            serde_json::Value::Array(arr) => arr.iter().any(|v| v.as_str() == Some(held)),
            _ => false,
        };
        if !matched {
            return false;
        }
    }
    true
}

fn apply_functions(
    funcs: &[LootFunction],
    ctx: &EvalContext<'_>,
    rng: &mut impl Rng,
    count: &mut i32,
) {
    for func in funcs {
        if !all_conditions(&func.conditions, ctx, rng) {
            continue;
        }
        match func.function.as_str() {
            "minecraft:set_count" => {
                if let Some(provider) = &func.count {
                    let value = provider.sample_int(rng);
                    if func.add {
                        *count = count.saturating_add(value);
                    } else {
                        *count = value;
                    }
                }
            }
            "minecraft:limit_count" => {
                if let Some(limit) = &func.limit {
                    if let Some(min) = limit.min {
                        *count = (*count).max(min.round() as i32);
                    }
                    if let Some(max) = limit.max {
                        *count = (*count).min(max.round() as i32);
                    }
                }
            }
            // explosion_decay, copy_components, apply_bonus (no Fortune), copy_state
            _ => {}
        }
    }
}

fn push_item_drops(item_name: &str, mut count: i32, out: &mut Vec<ItemStack>) {
    if count <= 0 {
        return;
    }
    let Some(def) = item_by_name(item_name) else {
        return;
    };
    let max_stack = i32::from(def.max_stack.max(1));
    while count > 0 {
        let take = count.min(max_stack) as u8;
        if let Some(stack) = ItemStack::new(def.id, take) {
            out.push(stack);
        }
        count -= i32::from(take);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};
    use std::collections::BTreeMap;

    #[test]
    fn official_loot_tables_cover_all_982_blocks_and_evaluate_accurately() {
        assert_eq!(BLOCK_LOOT_TABLES.len(), 982);
        let mut rng = StdRng::seed_from_u64(42);

        // Stone -> cobblestone
        let stone = crate::game::block_by_name("stone").unwrap().default_state;
        let drops = block_drops_with_rng(stone, None, &mut rng);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].definition().name, "minecraft:cobblestone");

        // Grass block -> dirt
        let grass = crate::game::block_by_name("grass_block")
            .unwrap()
            .default_state;
        let drops = block_drops_with_rng(grass, None, &mut rng);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].definition().name, "minecraft:dirt");

        // Glass without silk touch -> empty
        let glass = crate::game::block_by_name("glass").unwrap().default_state;
        assert!(block_drops_with_rng(glass, None, &mut rng).is_empty());

        // Coal ore -> coal
        let coal_ore = crate::game::block_by_name("coal_ore")
            .unwrap()
            .default_state;
        let drops = block_drops_with_rng(coal_ore, None, &mut rng);
        assert_eq!(drops[0].definition().name, "minecraft:coal");

        // Double oak slab -> 2 oak_slab
        let mut props = BTreeMap::new();
        props.insert("type".into(), "double".into());
        let double_slab = registry()
            .resolve_state("minecraft:oak_slab", &props)
            .unwrap();
        let drops = block_drops_with_rng(double_slab, None, &mut rng);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].definition().name, "minecraft:oak_slab");
        assert_eq!(drops[0].count, 2);

        // Oak leaves with shears -> oak_leaves
        let shears = item_by_name("shears").unwrap().id;
        let oak_leaves = crate::game::block_by_name("oak_leaves")
            .unwrap()
            .default_state;
        let drops = block_drops_with_rng(oak_leaves, Some(shears), &mut rng);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].definition().name, "minecraft:oak_leaves");

        // Oak door: lower half drops door, upper half drops nothing
        let mut lower_props = BTreeMap::new();
        lower_props.insert("half".into(), "lower".into());
        let lower_door = registry()
            .resolve_state("minecraft:oak_door", &lower_props)
            .unwrap();
        let mut upper_props = BTreeMap::new();
        upper_props.insert("half".into(), "upper".into());
        let upper_door = registry()
            .resolve_state("minecraft:oak_door", &upper_props)
            .unwrap();
        assert_eq!(block_drops_with_rng(lower_door, None, &mut rng).len(), 1);
        assert!(block_drops_with_rng(upper_door, None, &mut rng).is_empty());
    }
}
