#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Air,
    Stone,
    GrassBlock,
    Dirt,
    Bedrock,
    Glass,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKind {
    Pickaxe,
    Shovel,
    Axe,
    Hoe,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MiningLevel {
    Wood,
    Stone,
    Iron,
    Diamond,
    Netherite,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MiningRequirement {
    pub tool: ToolKind,
    pub level: MiningLevel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockDefinition {
    pub kind: BlockKind,
    pub name: &'static str,
    /// Global block-state ID for the one state currently implemented.
    pub state: i32,
    pub hardness: Option<f32>,
    /// `None` is not mineable. Breakable blocks require an exact tool kind and
    /// a tool level greater than or equal to this level.
    pub mining: Option<MiningRequirement>,
    pub replaceable: bool,
    pub collision: bool,
    pub blocks_skylight: bool,
}
const fn requirement(tool: ToolKind, level: MiningLevel) -> Option<MiningRequirement> {
    Some(MiningRequirement { tool, level })
}
pub const BLOCKS: [BlockDefinition; 6] = [
    BlockDefinition {
        kind: BlockKind::Air,
        name: "air",
        state: 0,
        hardness: Some(0.0),
        mining: None,
        replaceable: true,
        collision: false,
        blocks_skylight: false,
    },
    BlockDefinition {
        kind: BlockKind::Stone,
        name: "stone",
        state: 1,
        hardness: Some(1.5),
        mining: requirement(ToolKind::Pickaxe, MiningLevel::Wood),
        replaceable: false,
        collision: true,
        blocks_skylight: true,
    },
    BlockDefinition {
        kind: BlockKind::GrassBlock,
        name: "grass_block",
        state: 9,
        hardness: Some(0.6),
        mining: requirement(ToolKind::Shovel, MiningLevel::Wood),
        replaceable: false,
        collision: true,
        blocks_skylight: true,
    },
    BlockDefinition {
        kind: BlockKind::Dirt,
        name: "dirt",
        state: 10,
        hardness: Some(0.5),
        mining: requirement(ToolKind::Shovel, MiningLevel::Wood),
        replaceable: false,
        collision: true,
        blocks_skylight: true,
    },
    BlockDefinition {
        kind: BlockKind::Bedrock,
        name: "bedrock",
        state: 79,
        hardness: None,
        mining: None,
        replaceable: false,
        collision: true,
        blocks_skylight: true,
    },
    // A stone-tier gate exercises level enforcement in the initial subset.
    BlockDefinition {
        kind: BlockKind::Glass,
        name: "glass",
        state: 519,
        hardness: Some(0.3),
        mining: requirement(ToolKind::Pickaxe, MiningLevel::Stone),
        replaceable: false,
        collision: true,
        blocks_skylight: false,
    },
];
impl BlockKind {
    pub const fn definition(self) -> &'static BlockDefinition {
        &BLOCKS[self as usize]
    }
    pub const fn state(self) -> i32 {
        self.definition().state
    }
}
pub fn blocks() -> &'static [BlockDefinition] {
    &BLOCKS
}
pub fn block_by_state(state: i32) -> Option<&'static BlockDefinition> {
    BLOCKS.iter().find(|block| block.state == state)
}
pub fn block_by_name(name: &str) -> Option<&'static BlockDefinition> {
    BLOCKS.iter().find(|block| block.name == name)
}
