//! Registry syntax with an explicit resolver for this server's existing flat
//! world states and registered inventory items. Not a complete vanilla registry.
use super::{ResourceLocation, scalar};
use crate::command::{
    ArgumentKind, ArgumentParser, ArgumentValue, CommandError, CommandNode, StringReader,
    Suggestion, SuggestionContext,
};
use std::{collections::BTreeMap, fmt, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockStateSpec {
    pub name: ResourceLocation,
    pub properties: BTreeMap<String, String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemSpec {
    pub name: ResourceLocation,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryValueError {
    UnsupportedBlock,
    UnsupportedItem,
    UnsupportedProperty,
}
impl fmt::Display for RegistryValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedBlock => "Block is unavailable in the current world implementation.",
            Self::UnsupportedItem => "Item is unavailable in the current item registry.",
            Self::UnsupportedProperty => {
                "Block property/state is unavailable in the current world implementation."
            }
        })
    }
}
impl std::error::Error for RegistryValueError {}
impl BlockStateSpec {
    /// Protocol block-state ID, not block registry ID. No world mutation occurs.
    pub fn resolve_existing(&self) -> Result<i32, RegistryValueError> {
        if self.name.namespace != "minecraft" {
            return Err(RegistryValueError::UnsupportedBlock);
        }
        if crate::game::block_by_name(&self.name.path).is_none() {
            return Err(RegistryValueError::UnsupportedBlock);
        }
        crate::game::registry()
            .resolve_state(&self.name.path, &self.properties)
            .ok_or(RegistryValueError::UnsupportedProperty)
    }
}
impl ItemSpec {
    /// Resolve to a registered `Item` handle.
    pub fn resolve_existing(&self) -> Result<crate::game::Item, RegistryValueError> {
        if self.name.namespace != "minecraft" {
            return Err(RegistryValueError::UnsupportedItem);
        }
        crate::game::Item::by_name(&self.name.path).ok_or(RegistryValueError::UnsupportedItem)
    }
}
#[derive(Debug, Clone, Copy)]
pub struct BlockArgument;
#[derive(Debug, Clone, Copy)]
pub struct ItemArgument;
impl BlockArgument {
    pub fn kind(self) -> ArgumentKind {
        ArgumentKind::Custom(Arc::new(self))
    }
    pub fn node(self, name: impl Into<String>) -> CommandNode {
        CommandNode::argument(name, self.kind()).suggests(suggest_blocks)
    }
}
impl ItemArgument {
    pub fn kind(self) -> ArgumentKind {
        ArgumentKind::Custom(Arc::new(self))
    }
    pub fn node(self, name: impl Into<String>) -> CommandNode {
        CommandNode::argument(name, self.kind()).suggests(suggest_items)
    }
}
fn space(r: &mut StringReader<'_>) {
    while r.peek() == Some(32) {
        r.read();
    }
}
fn expect(r: &mut StringReader<'_>, c: u8) -> Result<(), CommandError> {
    if r.peek() == Some(c.into()) {
        r.read();
        Ok(())
    } else {
        Err(r.error(format!("Expected '{}'.", c as char)))
    }
}
fn identifier(r: &mut StringReader<'_>) -> Result<ResourceLocation, CommandError> {
    let start = r.cursor();
    let name = scalar::read_resource(r)?;
    if start == r.cursor() || name.path.is_empty() {
        return Err(r.error("Expected a registry identifier."));
    }
    Ok(name)
}
impl ArgumentParser for BlockArgument {
    fn parse(&self, r: &mut StringReader<'_>) -> Result<ArgumentValue, CommandError> {
        if r.remaining().len() > 4096 {
            return Err(r.error("Block argument exceeds limit."));
        }
        let name = identifier(r)?;
        let mut properties = BTreeMap::new();
        if r.peek() == Some(91) {
            r.read();
            space(r);
            while r.peek() != Some(93) {
                if properties.len() == 32 {
                    return Err(r.error("Too many block properties."));
                }
                let key = r.read_string()?;
                if key.is_empty() {
                    return Err(r.error("Missing property name."));
                }
                space(r);
                expect(r, b'=')?;
                space(r);
                let value = r.read_string()?;
                if value.is_empty() {
                    return Err(r.error("Missing property value."));
                }
                if properties.insert(key, value).is_some() {
                    return Err(r.error("Duplicate block property."));
                }
                space(r);
                if r.peek() == Some(44) {
                    r.read();
                    space(r);
                } else if r.peek() != Some(93) {
                    return Err(r.error("Expected ',' or ']'."));
                }
            }
            expect(r, b']')?;
        }
        if r.peek() == Some(123) {
            return Err(r.error("Block-entity NBT is not supported."));
        }
        Ok(ArgumentValue::Custom(Arc::new(BlockStateSpec {
            name,
            properties,
        })))
    }
    fn wire(&self) -> Option<(i32, Vec<u8>)> {
        Some((12, vec![]))
    }
    fn examples(&self) -> &[&str] {
        &["stone", "minecraft:grass_block[snowy=false]"]
    }
}
impl ArgumentParser for ItemArgument {
    fn parse(&self, r: &mut StringReader<'_>) -> Result<ArgumentValue, CommandError> {
        if r.remaining().len() > 4096 {
            return Err(r.error("Item argument exceeds limit."));
        }
        let name = identifier(r)?;
        if r.peek() == Some(91) {
            r.read();
            space(r);
            if r.peek() != Some(93) {
                return Err(r.error("Item components and component removal are not supported."));
            }
            r.read();
        }
        if r.peek() == Some(123) {
            return Err(r.error("Legacy item NBT is not supported."));
        }
        Ok(ArgumentValue::Custom(Arc::new(ItemSpec { name })))
    }
    fn wire(&self) -> Option<(i32, Vec<u8>)> {
        Some((14, vec![]))
    }
    fn examples(&self) -> &[&str] {
        &["stone", "minecraft:glass"]
    }
}
fn suggest(ctx: &SuggestionContext<'_>, names: &[&str]) -> Vec<Suggestion> {
    let prefix = ctx.remaining();
    names
        .iter()
        .flat_map(|name| [name.to_string(), format!("minecraft:{name}")])
        .filter(|name| name.starts_with(&prefix) && *name != prefix)
        .map(|name| ctx.suggest(name))
        .collect()
}
fn suggest_blocks(ctx: &SuggestionContext<'_>) -> Vec<Suggestion> {
    let names: Vec<_> = crate::game::blocks()
        .iter()
        .filter_map(|block| block.name.strip_prefix("minecraft:"))
        .collect();
    suggest(ctx, &names)
}
fn suggest_items(ctx: &SuggestionContext<'_>) -> Vec<Suggestion> {
    let names: Vec<_> = crate::game::items()
        .iter()
        .filter_map(|item| item.name.strip_prefix("minecraft:"))
        .collect();
    suggest(ctx, &names)
}
