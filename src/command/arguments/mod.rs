//! Typed Minecraft Java 1.21.1 command arguments, independent of world access.
mod block_items;
mod coordinates;
mod entities;
mod scalar;

pub use block_items::{BlockArgument, BlockStateSpec, ItemArgument, ItemSpec, RegistryValueError};
pub use entities::{
    EntityArgument, EntityGameMode, EntityQuery, EntityQueryError, EntitySnapshot, Selector,
    SelectorSort, Target,
};

pub use coordinates::{
    Angle, AxisCoordinate, BlockPos, ColumnPos, CoordinateError, Coordinates3, Rotation, Vec2, Vec3,
};
pub use scalar::{Axes, FloatRange, IntRange, ResourceLocation};

use super::{
    ArgumentKind, ArgumentParser, ArgumentValue, CommandError, CommandNode, StringReader,
    Suggestion, SuggestionContext,
};
use std::{any::Any, sync::Arc};

/// Minecraft argument descriptors for protocol 767. Centering is server-side;
/// the protocol has no properties for any of these argument types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinecraftArgument {
    ResourceLocation,
    Uuid,
    IntRange,
    FloatRange,
    Angle,
    Rotation,
    Swizzle,
    EntityAnchor,
    BlockPos,
    ColumnPos,
    Vec3 { center_correct: bool },
    Vec2 { center_correct: bool },
}

impl MinecraftArgument {
    pub fn kind(self) -> ArgumentKind {
        ArgumentKind::Custom(Arc::new(self))
    }

    /// Register with generic suggestions when the argument has static options.
    /// Custom suggestions can override these via `CommandNode::suggests`.
    pub fn node(self, name: impl Into<String>) -> CommandNode {
        let node = CommandNode::argument(name, self.kind());
        match self {
            Self::EntityAnchor => node.suggests(suggest_anchor),
            Self::Swizzle => node.suggests(suggest_axes),
            Self::BlockPos | Self::Vec3 { .. } => node.suggests(suggest_pos3),
            Self::ColumnPos | Self::Vec2 { .. } | Self::Rotation => node.suggests(suggest_pos2),
            Self::Angle => node.suggests(suggest_angle),
            _ => node,
        }
    }
}
impl From<MinecraftArgument> for ArgumentKind {
    fn from(value: MinecraftArgument) -> Self {
        value.kind()
    }
}

impl ArgumentParser for MinecraftArgument {
    fn parse(&self, reader: &mut StringReader<'_>) -> Result<ArgumentValue, CommandError> {
        fn typed(value: impl Any + Send + Sync) -> ArgumentValue {
            ArgumentValue::Custom(Arc::new(value))
        }
        Ok(match *self {
            Self::ResourceLocation => typed(scalar::read_resource(reader)?),
            Self::Uuid => typed(scalar::read_uuid(reader)?),
            Self::IntRange => typed(scalar::read_int_range(reader)?),
            Self::FloatRange => typed(scalar::read_float_range(reader)?),
            Self::Angle => typed(coordinates::read_angle(reader)?),
            Self::Rotation => typed(Rotation(coordinates::read_world(
                reader, [false; 2], false,
            )?)),
            Self::Swizzle => typed(scalar::read_axes(reader)?),
            Self::EntityAnchor => typed(scalar::read_anchor(reader)?),
            Self::BlockPos => typed(BlockPos(coordinates::read_3d(reader, false, true)?)),
            Self::ColumnPos => typed(ColumnPos(coordinates::read_world(
                reader, [false; 2], true,
            )?)),
            Self::Vec3 { center_correct } => {
                typed(Vec3(coordinates::read_3d(reader, center_correct, false)?))
            }
            Self::Vec2 { center_correct } => typed(Vec2(coordinates::read_world(
                reader,
                [center_correct; 2],
                false,
            )?)),
        })
    }

    fn examples(&self) -> &[&str] {
        match self {
            Self::ResourceLocation => &["stone", "minecraft:stone", "foo:bar/baz"],
            Self::Uuid => &["dd12be42-52a9-4a91-a8a1-11c01849e498"],
            Self::IntRange => &["0", "0..5", "..5", "0.."],
            Self::FloatRange => &["0", "0.5..1.5", "..5", "0.."],
            Self::Angle => &["0", "~", "~-5"],
            Self::Rotation => &["0 0", "~ ~", "~-5 ~5"],
            Self::Swizzle => &["xyz", "x"],
            Self::EntityAnchor => &["feet", "eyes"],
            Self::BlockPos | Self::Vec3 { .. } => &["0 0 0", "~ ~ ~", "^ ^ ^", "~0.5 ~1 ~-5"],
            Self::ColumnPos => &["0 0", "~ ~", "~1 ~-2"],
            Self::Vec2 { .. } => &["0 0", "~ ~", "0.1 -0.5"],
        }
    }

    fn wire(&self) -> Option<(i32, Vec<u8>)> {
        Some((
            match self {
                Self::BlockPos => 8,
                Self::ColumnPos => 9,
                Self::Vec3 { .. } => 10,
                Self::Vec2 { .. } => 11,
                Self::Angle => 27,
                Self::Rotation => 28,
                Self::Swizzle => 31,
                Self::ResourceLocation => 35,
                Self::EntityAnchor => 37,
                Self::IntRange => 38,
                Self::FloatRange => 39,
                Self::Uuid => 53,
            },
            Vec::new(),
        ))
    }
}

fn suggest_prefix(ctx: &SuggestionContext<'_>, candidates: &[&str]) -> Vec<Suggestion> {
    let remaining = ctx.remaining();
    candidates
        .iter()
        .filter(|&&text| text != remaining && text.starts_with(&remaining))
        .map(|&text| ctx.suggest(text))
        .collect()
}
fn suggest_anchor(ctx: &SuggestionContext<'_>) -> Vec<Suggestion> {
    suggest_prefix(ctx, &["feet", "eyes"])
}
fn suggest_axes(ctx: &SuggestionContext<'_>) -> Vec<Suggestion> {
    suggest_prefix(
        ctx,
        &[
            "x", "y", "z", "xy", "xz", "yx", "yz", "zx", "zy", "xyz", "xzy", "yxz", "yzx", "zxy",
            "zyx",
        ],
    )
}
fn suggest_pos3(ctx: &SuggestionContext<'_>) -> Vec<Suggestion> {
    suggest_prefix(ctx, &["~ ~ ~", "^ ^ ^"])
}
fn suggest_pos2(ctx: &SuggestionContext<'_>) -> Vec<Suggestion> {
    suggest_prefix(ctx, &["~ ~"])
}
fn suggest_angle(ctx: &SuggestionContext<'_>) -> Vec<Suggestion> {
    suggest_prefix(ctx, &["~"])
}

fn take(reader: &mut StringReader<'_>, predicate: impl Fn(u16) -> bool) -> String {
    let count = reader
        .remaining()
        .iter()
        .copied()
        .take_while(|&c| predicate(c))
        .count();
    let value = String::from_utf16_lossy(&reader.remaining()[..count]);
    reader.set_cursor(reader.cursor() + count);
    value
}

fn take_char(reader: &mut StringReader<'_>, byte: u8) -> bool {
    if reader.peek() == Some(u16::from(byte)) {
        reader.read();
        true
    } else {
        false
    }
}
