//! Vanilla 1.21.1 survival block-break loot table resolution.
use super::{ItemStack, item_by_name, registry};
use rand::Rng;

fn single_drop(name: &str, count: u8) -> Vec<ItemStack> {
    item_by_name(name)
        .and_then(|def| ItemStack::new(def.id, count))
        .into_iter()
        .collect()
}

/// Resolve the Survival mode item drops for breaking a block with `state`
/// (assuming no Silk Touch / Fortune).
pub fn block_drops(state: i32) -> Vec<ItemStack> {
    let mut rng = rand::thread_rng();
    block_drops_with_rng(state, &mut rng)
}

pub fn block_drops_with_rng(state: i32, rng: &mut impl Rng) -> Vec<ItemStack> {
    let Some((_, block_def, state_def)) = registry().state_by_id(state) else {
        return Vec::new();
    };
    let path = block_def
        .name
        .strip_prefix("minecraft:")
        .unwrap_or(&block_def.name);

    match path {
        // Stone / soil transforms
        "stone" => single_drop("cobblestone", 1),
        "deepslate" => single_drop("cobbled_deepslate", 1),
        "grass_block" | "mycelium" | "podzol" | "dirt_path" | "farmland" => single_drop("dirt", 1),
        "crimson_nylium" | "warped_nylium" => single_drop("netherrack", 1),

        // Ores
        "coal_ore" | "deepslate_coal_ore" => single_drop("coal", 1),
        "iron_ore" | "deepslate_iron_ore" => single_drop("raw_iron", 1),
        "copper_ore" | "deepslate_copper_ore" => single_drop("raw_copper", rng.gen_range(2..=5)),
        "gold_ore" | "deepslate_gold_ore" => single_drop("raw_gold", 1),
        "diamond_ore" | "deepslate_diamond_ore" => single_drop("diamond", 1),
        "emerald_ore" | "deepslate_emerald_ore" => single_drop("emerald", 1),
        "lapis_ore" | "deepslate_lapis_ore" => single_drop("lapis_lazuli", rng.gen_range(4..=9)),
        "redstone_ore" | "deepslate_redstone_ore" => single_drop("redstone", rng.gen_range(4..=5)),
        "nether_quartz_ore" => single_drop("quartz", 1),
        "nether_gold_ore" => single_drop("gold_nugget", rng.gen_range(2..=6)),

        // Multi-item blocks
        "glowstone" => single_drop("glowstone_dust", rng.gen_range(2..=4)),
        "sea_lantern" => single_drop("prismarine_crystals", rng.gen_range(2..=3)),
        "clay" => single_drop("clay_ball", 4),
        "snow_block" => single_drop("snowball", 4),
        "snow" => {
            let layers = state_def
                .properties
                .get("layers")
                .and_then(|v| v.parse::<u8>().ok())
                .unwrap_or(1)
                .clamp(1, 8);
            single_drop("snowball", layers)
        }
        "bookshelf" => single_drop("book", 3),
        "melon" => single_drop("melon_slice", rng.gen_range(3..=7)),
        "campfire" => single_drop("charcoal", 2),
        "soul_campfire" => single_drop("soul_soil", 1),

        // Probabilistic blocks
        "gravel" => {
            if rng.gen_ratio(1, 10) {
                single_drop("flint", 1)
            } else {
                single_drop("gravel", 1)
            }
        }
        "short_grass" | "tall_grass" | "fern" | "large_fern" => {
            if rng.gen_ratio(1, 8) {
                single_drop("wheat_seeds", 1)
            } else {
                Vec::new()
            }
        }

        // Fragile / no-drop without Silk Touch
        "glass" | "glass_pane" | "tinted_glass" | "ice" | "packed_ice" | "blue_ice" | "spawner"
        | "trial_spawner" | "ender_chest" => {
            if path == "ender_chest" {
                single_drop("obsidian", 8)
            } else {
                Vec::new()
            }
        }
        _ if path.ends_with("_stained_glass")
            || path.ends_with("_stained_glass_pane")
            || path.starts_with("infested_") =>
        {
            Vec::new()
        }

        // Leaves: sapling / stick / apple
        _ if path.ends_with("_leaves") => {
            let mut out = Vec::new();
            let sapling_chance = if path == "jungle_leaves" { 0.025 } else { 0.05 };
            if rng.r#gen::<f64>() < sapling_chance {
                let sapling_name = match path {
                    "azalea_leaves" => Some("azalea"),
                    "flowering_azalea_leaves" => Some("flowering_azalea"),
                    "mangrove_leaves" => Some("mangrove_propagule"),
                    _ => None,
                };
                if let Some(name) = sapling_name {
                    out.extend(single_drop(name, 1));
                } else {
                    let prefix = path.trim_end_matches("_leaves");
                    out.extend(single_drop(&format!("{prefix}_sapling"), 1));
                }
            }
            if rng.r#gen::<f64>() < 0.02 {
                out.extend(single_drop("stick", rng.gen_range(1..=2)));
            }
            if matches!(path, "oak_leaves" | "dark_oak_leaves") && rng.r#gen::<f64>() < 0.005 {
                out.extend(single_drop("apple", 1));
            }
            out
        }

        // Double slabs drop 2 slabs
        _ if state_def.properties.get("type").map(String::as_str) == Some("double")
            && path.ends_with("_slab") =>
        {
            block_def
                .item
                .as_deref()
                .map(|name| single_drop(name, 2))
                .unwrap_or_default()
        }

        // Default: drop 1 of the block's own item.
        _ => block_def
            .item
            .as_deref()
            .map(|name| single_drop(name, 1))
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    #[test]
    fn vanilla_block_drops_match_expected_items() {
        let mut rng = StdRng::seed_from_u64(42);
        let stone = crate::game::block_by_name("stone").unwrap().default_state;
        let drops = block_drops_with_rng(stone, &mut rng);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].definition().name, "minecraft:cobblestone");

        let grass = crate::game::block_by_name("grass_block")
            .unwrap()
            .default_state;
        let drops = block_drops_with_rng(grass, &mut rng);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].definition().name, "minecraft:dirt");

        let glass = crate::game::block_by_name("glass").unwrap().default_state;
        assert!(block_drops_with_rng(glass, &mut rng).is_empty());

        let coal_ore = crate::game::block_by_name("coal_ore")
            .unwrap()
            .default_state;
        let drops = block_drops_with_rng(coal_ore, &mut rng);
        assert_eq!(drops[0].definition().name, "minecraft:coal");
    }
}
