//! Vanilla-aligned block placement state resolution (stairs, slabs, logs,
//! torches, directional blocks, trapdoors, signs, etc.).
use crate::game::{BlockDefinition, registry};
use std::collections::BTreeMap;

/// Context of a player's block placement click.
#[derive(Debug, Clone, Copy)]
pub struct PlacementContext {
    pub yaw: f32,
    pub pitch: f32,
    /// Clicked face of the support block: 0=-Y, 1=+Y, 2=-Z(N), 3=+Z(S), 4=-X(W), 5=+X(E).
    pub face: i32,
    /// Cursor hit offset within the clicked block (`0.0..=1.0`).
    pub hit: [f32; 3],
    /// Current block state at the support position (for slab merging).
    pub support_state: i32,
    /// Current block state at the target placement position (for slab merging).
    pub target_state: i32,
}

/// Result of resolving a block placement: either place into `target` or merge into `support` (double slab).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementResult {
    PlaceTarget(i32),
    MergeSupport(i32),
}

/// Cardinal direction the player is looking horizontally (`north`/`south`/`east`/`west`).
pub fn horizontal_facing(yaw: f32) -> &'static str {
    let wrapped = yaw.rem_euclid(360.0);
    if !(45.0..315.0).contains(&wrapped) {
        "south"
    } else if wrapped < 135.0 {
        "west"
    } else if wrapped < 225.0 {
        "north"
    } else {
        "east"
    }
}

pub fn opposite_facing(dir: &str) -> &'static str {
    match dir {
        "north" => "south",
        "south" => "north",
        "east" => "west",
        "west" => "east",
        "up" => "down",
        "down" => "up",
        _ => "north",
    }
}

fn face_to_direction(face: i32) -> Option<&'static str> {
    match face {
        0 => Some("down"),
        1 => Some("up"),
        2 => Some("north"),
        3 => Some("south"),
        4 => Some("west"),
        5 => Some("east"),
        _ => None,
    }
}

fn face_to_axis(face: i32) -> &'static str {
    match face {
        0 | 1 => "y",
        2 | 3 => "z",
        4 | 5 => "x",
        _ => "y",
    }
}

/// Resolve the exact block state ID to place given the held block and click context.
pub fn resolve_placement(block_index: usize, ctx: PlacementContext) -> Option<PlacementResult> {
    let reg = registry();
    let block = reg.blocks().get(block_index)?;

    // 1. Check slab merging into the clicked support block first.
    if is_slab(block)
        && let Some((_, support_def, support_state_def)) = reg.state_by_id(ctx.support_state)
        && support_def.name == block.name
        && let Some(existing_type) = support_state_def.properties.get("type")
    {
        let can_merge = (existing_type == "bottom"
            && (ctx.face == 1 || (ctx.face >= 2 && ctx.hit[1] > 0.5)))
            || (existing_type == "top" && (ctx.face == 0 || (ctx.face >= 2 && ctx.hit[1] <= 0.5)));
        if can_merge {
            let mut props = BTreeMap::new();
            props.insert("type".into(), "double".into());
            if let Some(double_state) = reg.resolve_state(&block.name, &props) {
                return Some(PlacementResult::MergeSupport(double_state));
            }
        }
    }

    // 2. Check slab merging into the target block (when clicking a side/adjacent block into a half-slab).
    if is_slab(block)
        && let Some((_, target_def, target_state_def)) = reg.state_by_id(ctx.target_state)
        && target_def.name == block.name
        && target_state_def
            .properties
            .get("type")
            .is_some_and(|t| t != "double")
    {
        let mut props = BTreeMap::new();
        props.insert("type".into(), "double".into());
        if let Some(double_state) = reg.resolve_state(&block.name, &props) {
            return Some(PlacementResult::PlaceTarget(double_state));
        }
    }

    // 3. Wall-mounted torch variants when placed on a horizontal face (2..=5).
    if ctx.face >= 2
        && let Some(wall_name) = wall_variant(&block.name)
        && let Some(dir) = face_to_direction(ctx.face)
    {
        let mut props = BTreeMap::new();
        props.insert("facing".into(), dir.into());
        if let Some(state) = reg.resolve_state(wall_name, &props) {
            return Some(PlacementResult::PlaceTarget(state));
        }
    }

    // 4. General property resolution based on the block's declared properties.
    let mut props = BTreeMap::new();

    if block.properties.contains_key("axis") {
        let axis = face_to_axis(ctx.face);
        if has_prop_value(block, "axis", axis) {
            props.insert("axis".into(), axis.into());
        }
    }

    if is_slab(block) {
        let slab_type = if ctx.face == 0 || (ctx.face >= 2 && ctx.hit[1] > 0.5) {
            "top"
        } else {
            "bottom"
        };
        props.insert("type".into(), slab_type.into());
    }

    if block.properties.contains_key("half")
        && has_prop_value(block, "half", "top")
        && has_prop_value(block, "half", "bottom")
    {
        let half = if ctx.face == 0 || (ctx.face >= 2 && ctx.hit[1] > 0.5) {
            "top"
        } else {
            "bottom"
        };
        props.insert("half".into(), half.into());
    }

    if block.properties.contains_key("facing") {
        let dir = resolve_facing(block, ctx);
        if has_prop_value(block, "facing", dir) {
            props.insert("facing".into(), dir.into());
        }
    }

    if block.properties.contains_key("rotation") {
        let rot = (((f64::from(ctx.yaw) + 180.0) * 16.0 / 360.0) + 0.5)
            .floor()
            .rem_euclid(16.0) as u8;
        let rot_str = rot.to_string();
        if has_prop_value(block, "rotation", &rot_str) {
            props.insert("rotation".into(), rot_str);
        }
    }

    let state = if props.is_empty() {
        block.default_state
    } else {
        reg.resolve_state(&block.name, &props)
            .unwrap_or(block.default_state)
    };
    Some(PlacementResult::PlaceTarget(state))
}

fn has_prop_value(block: &BlockDefinition, key: &str, value: &str) -> bool {
    block
        .properties
        .get(key)
        .is_some_and(|vals| vals.iter().any(|v| v == value))
}

fn is_slab(block: &BlockDefinition) -> bool {
    block.properties.contains_key("type")
        && has_prop_value(block, "type", "top")
        && has_prop_value(block, "type", "bottom")
        && has_prop_value(block, "type", "double")
}

fn wall_variant(name: &str) -> Option<&'static str> {
    match name {
        "minecraft:torch" => Some("minecraft:wall_torch"),
        "minecraft:soul_torch" => Some("minecraft:soul_wall_torch"),
        "minecraft:redstone_torch" => Some("minecraft:redstone_wall_torch"),
        _ => None,
    }
}

fn resolve_facing(block: &BlockDefinition, ctx: PlacementContext) -> &'static str {
    let supports_vertical =
        has_prop_value(block, "facing", "up") && has_prop_value(block, "facing", "down");
    let look = horizontal_facing(ctx.yaw);

    // Blocks that attach directly to the clicked face.
    let attaches_to_face = block.name.ends_with("_wall_torch")
        || block.name.ends_with("_button")
        || block.name.ends_with("_rod")
        || block.name == "minecraft:ladder"
        || block.name == "minecraft:lever"
        || block.name == "minecraft:tripwire_hook"
        || block.name == "minecraft:lightning_rod";
    if attaches_to_face
        && let Some(dir) = face_to_direction(ctx.face)
        && has_prop_value(block, "facing", dir)
    {
        return dir;
    }

    if supports_vertical {
        if ctx.pitch > 50.0 {
            return "up";
        }
        if ctx.pitch < -50.0 {
            return "down";
        }
        return opposite_facing(look);
    }

    // Stairs and fence gates face the same direction the player is looking.
    if block.name.ends_with("_stairs")
        || block.name.ends_with("_fence_gate")
        || block.name.ends_with("_door")
    {
        return look;
    }

    // Chests, furnaces, trapdoors, carved pumpkins, repeaters, etc. face toward the player.
    opposite_facing(look)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(yaw: f32, pitch: f32, face: i32, hit_y: f32) -> PlacementContext {
        PlacementContext {
            yaw,
            pitch,
            face,
            hit: [0.5, hit_y, 0.5],
            support_state: crate::game::STONE_STATE,
            target_state: crate::game::AIR_STATE,
        }
    }

    #[test]
    fn resolves_stairs_slabs_logs_wall_torches_and_chests() {
        let reg = registry();

        // Oak stairs looking east (yaw = -90 / 270) on upper half of side face -> facing=east, half=top.
        let (stairs_idx, _) = reg.block_by_name("oak_stairs").unwrap();
        let PlacementResult::PlaceTarget(state) =
            resolve_placement(stairs_idx, ctx(270.0, 0.0, 2, 0.8)).unwrap()
        else {
            panic!("expected PlaceTarget");
        };
        let (_, _, def) = reg.state_by_id(state).unwrap();
        assert_eq!(
            def.properties.get("facing").map(String::as_str),
            Some("east")
        );
        assert_eq!(def.properties.get("half").map(String::as_str), Some("top"));

        // Oak log clicked on +X face (5) -> axis=x.
        let (log_idx, _) = reg.block_by_name("oak_log").unwrap();
        let PlacementResult::PlaceTarget(log_state) =
            resolve_placement(log_idx, ctx(0.0, 0.0, 5, 0.5)).unwrap()
        else {
            panic!("expected PlaceTarget");
        };
        assert_eq!(
            reg.state_by_id(log_state)
                .unwrap()
                .2
                .properties
                .get("axis")
                .map(String::as_str),
            Some("x")
        );

        // Torch placed on +X wall (face=5) -> wall_torch[facing=east].
        let (torch_idx, _) = reg.block_by_name("torch").unwrap();
        let PlacementResult::PlaceTarget(torch_state) =
            resolve_placement(torch_idx, ctx(0.0, 0.0, 5, 0.5)).unwrap()
        else {
            panic!("expected PlaceTarget");
        };
        let (_, wall_def, wall_state) = reg.state_by_id(torch_state).unwrap();
        assert_eq!(wall_def.name, "minecraft:wall_torch");
        assert_eq!(
            wall_state.properties.get("facing").map(String::as_str),
            Some("east")
        );

        // Chest placed while looking south (yaw=0) -> faces north (toward player).
        let (chest_idx, _) = reg.block_by_name("chest").unwrap();
        let PlacementResult::PlaceTarget(chest_state) =
            resolve_placement(chest_idx, ctx(0.0, 0.0, 1, 1.0)).unwrap()
        else {
            panic!("expected PlaceTarget");
        };
        assert_eq!(
            reg.state_by_id(chest_state)
                .unwrap()
                .2
                .properties
                .get("facing")
                .map(String::as_str),
            Some("north")
        );

        // Stone slab placed on top of bottom stone slab -> MergeSupport(double).
        let (slab_idx, slab_def) = reg.block_by_name("stone_slab").unwrap();
        let mut slab_ctx = ctx(0.0, 0.0, 1, 1.0);
        slab_ctx.support_state = slab_def.default_state; // bottom
        let PlacementResult::MergeSupport(double_state) =
            resolve_placement(slab_idx, slab_ctx).unwrap()
        else {
            panic!("expected MergeSupport");
        };
        assert_eq!(
            reg.state_by_id(double_state)
                .unwrap()
                .2
                .properties
                .get("type")
                .map(String::as_str),
            Some("double")
        );
    }
}
