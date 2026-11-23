/// Stable identity for the currently implemented block subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Air,
    Stone,
    GrassBlock,
    Dirt,
    Bedrock,
    Glass,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockDefinition {
    pub kind: BlockKind,
    pub name: &'static str,
    /// Global block-state ID for the one state currently implemented.
    pub state: i32,
    pub hardness: Option<f32>,
    pub replaceable: bool,
    pub collision: bool,
    pub blocks_skylight: bool,
}

pub const BLOCKS: [BlockDefinition; 6] = [
    BlockDefinition {
        kind: BlockKind::Air,
        name: "air",
        state: 0,
        hardness: Some(0.0),
        replaceable: true,
        collision: false,
        blocks_skylight: false,
    },
    BlockDefinition {
        kind: BlockKind::Stone,
        name: "stone",
        state: 1,
        hardness: Some(1.5),
        replaceable: false,
        collision: true,
        blocks_skylight: true,
    },
    BlockDefinition {
        kind: BlockKind::GrassBlock,
        name: "grass_block",
        state: 9,
        hardness: Some(0.6),
        replaceable: false,
        collision: true,
        blocks_skylight: true,
    },
    BlockDefinition {
        kind: BlockKind::Dirt,
        name: "dirt",
        state: 10,
        hardness: Some(0.5),
        replaceable: false,
        collision: true,
        blocks_skylight: true,
    },
    BlockDefinition {
        kind: BlockKind::Bedrock,
        name: "bedrock",
        state: 79,
        hardness: None,
        replaceable: false,
        collision: true,
        blocks_skylight: true,
    },
    BlockDefinition {
        kind: BlockKind::Glass,
        name: "glass",
        state: 519,
        hardness: Some(0.3),
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
