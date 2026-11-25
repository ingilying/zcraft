//! Generated Java 1.21.1 gameplay metadata and indexed runtime queries.
use serde::Deserialize;
use std::{collections::HashMap, sync::LazyLock};

#[derive(Debug, Deserialize)]
struct RawGameData {
    format: u32,
    minecraft: String,
    protocol: i32,
    server_sha1: String,
    blocks: Vec<BlockDefinition>,
    states: Vec<BlockStateDefinition>,
    block_tags: HashMap<String, Vec<usize>>,
    items: Vec<ItemDefinition>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct BlockDefinition {
    pub name: String,
    pub default_state: i32,
    pub first_state: usize,
    pub state_count: usize,
    pub properties: HashMap<String, Vec<String>>,
    pub item: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct BlockStateDefinition {
    pub id: i32,
    pub properties: HashMap<String, String>,
    pub hardness: f32,
    pub correct_tool: bool,
    pub air: bool,
    pub liquid: bool,
    pub occludes: bool,
    pub replaceable: bool,
    pub collision: bool,
    /// True when vanilla propagates skylight through this state.
    pub skylight: bool,
    /// Vanilla block-light emission level (0..=15).
    pub light_emission: u8,
    /// Vanilla light attenuation / opacity level (0..=15).
    pub light_block: u8,
}
#[derive(Debug, Clone, Deserialize)]
pub struct ToolRule {
    pub blocks: Vec<String>,
    pub speed: Option<f32>,
    pub correct_for_drops: Option<bool>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct ItemDefinition {
    pub name: String,
    pub id: i32,
    pub max_stack: u8,
    pub max_damage: u32,
    pub tool_rules: Vec<ToolRule>,
    pub block: Option<usize>,
}

pub struct GameRegistry {
    raw: RawGameData,
    block_names: HashMap<String, usize>,
    item_names: HashMap<String, usize>,
    item_ids: Vec<Option<usize>>,
    state_ids: Vec<Option<(usize, usize)>>,
}
impl GameRegistry {
    fn load() -> Self {
        const COMPRESSED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/game_data.json.zlib"));
        let mut decoder = flate2::read::ZlibDecoder::new(COMPRESSED);
        let mut json = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut json)
            .expect("compressed game data must be valid");
        let raw: RawGameData =
            serde_json::from_slice(&json).expect("generated game data must be valid");
        assert_eq!(
            (raw.format, raw.minecraft.as_str(), raw.protocol),
            (1, "1.21.1", 767)
        );
        assert_eq!(raw.server_sha1, "59353fb40c36d304f2035d51e7d6e6baa98dc05c");
        let block_names = raw
            .blocks
            .iter()
            .enumerate()
            .map(|(i, v)| (v.name.clone(), i))
            .collect();
        let item_names = raw
            .items
            .iter()
            .enumerate()
            .map(|(i, v)| (v.name.clone(), i))
            .collect();
        let mut item_ids =
            vec![None; raw.items.iter().map(|v| v.id).max().unwrap_or(0) as usize + 1];
        for (index, item) in raw.items.iter().enumerate() {
            item_ids[item.id as usize] = Some(index)
        }
        let mut state_ids =
            vec![None; raw.states.iter().map(|v| v.id).max().unwrap_or(0) as usize + 1];
        for (block_index, block) in raw.blocks.iter().enumerate() {
            let states = &raw.states[block.first_state..block.first_state + block.state_count];
            for (offset, state) in states.iter().enumerate() {
                state_ids[state.id as usize] = Some((block_index, offset));
            }
        }
        assert!(item_ids.iter().all(Option::is_some));
        assert!(state_ids.iter().all(Option::is_some));
        Self {
            raw,
            block_names,
            item_names,
            item_ids,
            state_ids,
        }
    }
    pub fn blocks(&self) -> &[BlockDefinition] {
        &self.raw.blocks
    }
    pub fn states(&self) -> &[BlockStateDefinition] {
        &self.raw.states
    }
    pub fn items(&self) -> &[ItemDefinition] {
        &self.raw.items
    }
    pub fn block_by_name(&self, name: &str) -> Option<(usize, &BlockDefinition)> {
        let name = qualified(name);
        let i = *self.block_names.get(name.as_ref())?;
        Some((i, &self.raw.blocks[i]))
    }
    pub fn item_by_name(&self, name: &str) -> Option<&ItemDefinition> {
        let name = qualified(name);
        self.item_names
            .get(name.as_ref())
            .map(|&i| &self.raw.items[i])
    }
    pub fn item_by_id(&self, id: i32) -> Option<&ItemDefinition> {
        usize::try_from(id)
            .ok()
            .and_then(|id| self.item_ids.get(id))
            .copied()
            .flatten()
            .map(|i| &self.raw.items[i])
    }
    pub fn state_by_id(&self, id: i32) -> Option<(usize, &BlockDefinition, &BlockStateDefinition)> {
        let (block, offset) = usize::try_from(id)
            .ok()
            .and_then(|id| self.state_ids.get(id))
            .copied()
            .flatten()?;
        let definition = &self.raw.blocks[block];
        Some((
            block,
            definition,
            &self.raw.states[definition.first_state + offset],
        ))
    }
    pub fn default_state(&self, block: usize) -> Option<i32> {
        self.raw.blocks.get(block).map(|v| v.default_state)
    }
    pub fn resolve_state(
        &self,
        name: &str,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Option<i32> {
        let (block_index, block) = self.block_by_name(name)?;
        let (_, _, default) = self.state_by_id(block.default_state)?;
        let mut properties = default.properties.clone();
        for (key, value) in overrides {
            if !block
                .properties
                .get(key)
                .is_some_and(|values| values.contains(value))
            {
                return None;
            }
            properties.insert(key.clone(), value.clone());
        }
        let states = &self.raw.states[block.first_state..block.first_state + block.state_count];
        let state = states.iter().find(|state| state.properties == properties)?;
        debug_assert_eq!(self.state_by_id(state.id)?.0, block_index);
        Some(state.id)
    }
    fn selector_matches(&self, block: usize, selector: &str) -> bool {
        if let Some(tag) = selector.strip_prefix('#') {
            self.raw
                .block_tags
                .get(tag)
                .is_some_and(|members| members.binary_search(&block).is_ok())
        } else {
            self.raw
                .blocks
                .get(block)
                .is_some_and(|value| value.name == selector)
        }
    }
    pub fn tool_speed(&self, item: Option<i32>, block: usize) -> f32 {
        let Some(item) = item.and_then(|id| self.item_by_id(id)) else {
            return 1.0;
        };
        for rule in &item.tool_rules {
            if rule.blocks.iter().any(|v| self.selector_matches(block, v))
                && let Some(speed) = rule.speed
            {
                return speed;
            }
        }
        1.0
    }
    pub fn correct_for_drops(&self, item: Option<i32>, block: usize) -> bool {
        let Some(item) = item.and_then(|id| self.item_by_id(id)) else {
            return false;
        };
        for rule in &item.tool_rules {
            if rule.blocks.iter().any(|v| self.selector_matches(block, v))
                && let Some(correct) = rule.correct_for_drops
            {
                return correct;
            }
        }
        false
    }
    /// Project policy: blocks in a vanilla mineable tool tag require a matching
    /// tool; tier gates come from vanilla's incorrect-tool rules.
    pub fn can_mine(&self, item: Option<i32>, block: usize, state: &BlockStateDefinition) -> bool {
        if state.hardness < 0.0 {
            return false;
        }
        let tagged = [
            "minecraft:mineable/pickaxe",
            "minecraft:mineable/shovel",
            "minecraft:mineable/axe",
            "minecraft:mineable/hoe",
        ]
        .iter()
        .any(|tag| {
            self.raw
                .block_tags
                .get(*tag)
                .is_some_and(|v| v.binary_search(&block).is_ok())
        });
        !tagged
            || (self.tool_speed(item, block) > 1.0
                && (!state.correct_tool || self.correct_for_drops(item, block)))
    }
    pub fn block_tags(&self) -> &HashMap<String, Vec<usize>> {
        &self.raw.block_tags
    }
}
fn qualified(name: &str) -> std::borrow::Cow<'_, str> {
    if name.contains(':') {
        name.into()
    } else {
        format!("minecraft:{name}").into()
    }
}
pub static GAME: LazyLock<GameRegistry> = LazyLock::new(GameRegistry::load);
pub fn registry() -> &'static GameRegistry {
    &GAME
}
pub fn blocks() -> &'static [BlockDefinition] {
    registry().blocks()
}
pub fn items() -> &'static [ItemDefinition] {
    registry().items()
}
pub fn block_by_name(name: &str) -> Option<&'static BlockDefinition> {
    registry().block_by_name(name).map(|(_, v)| v)
}
pub fn block_by_state(state: i32) -> Option<&'static BlockStateDefinition> {
    registry().state_by_id(state).map(|(_, _, v)| v)
}
pub fn item_by_name(name: &str) -> Option<&'static ItemDefinition> {
    registry().item_by_name(name)
}
pub fn item_by_id(id: i32) -> Option<&'static ItemDefinition> {
    registry().item_by_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_registry_is_complete_and_tool_rules_resolve() {
        assert_eq!(registry().blocks().len(), 1060);
        assert_eq!(registry().states().len(), 26684);
        assert_eq!(registry().items().len(), 1333);
        let (stone_i, stone) = registry().block_by_name("stone").unwrap();
        let state = registry().state_by_id(stone.default_state).unwrap().2;
        let wood = registry().item_by_name("wooden_pickaxe").unwrap();
        assert_eq!(registry().tool_speed(Some(wood.id), stone_i), 2.0);
        assert!(registry().can_mine(Some(wood.id), stone_i, state));
        let dirt_i = registry().block_by_name("dirt").unwrap().0;
        assert!(!registry().can_mine(Some(wood.id), dirt_i, registry().state_by_id(10).unwrap().2));
        assert_eq!(
            registry()
                .item_by_name("diamond_pickaxe")
                .unwrap()
                .max_damage,
            1561
        );
    }
}
