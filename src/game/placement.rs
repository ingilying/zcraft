//! Vanilla-aligned block placement state resolution (stairs + corner shapes,
//! slabs, logs, torches, directional blocks, trapdoors, signs, beds, doors,
//! double plants, waterlogging, and support attachment rules).
use crate::{
    game::{BlockDefinition, registry},
    world::BlockPos,
};
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
    /// Current block state at the target placement position (for slab merging / waterlogging).
    pub target_state: i32,
}

/// Result of resolving a block placement: place into `target`, merge into `support`
/// (double slab), or place a two-block structure (`PlacePair`: primary at target,
/// secondary at offset `[dx, dy, dz]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementResult {
    PlaceTarget(i32),
    MergeSupport(i32),
    PlacePair {
        primary: i32,
        secondary_offset: [i32; 3],
        secondary: i32,
    },
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

    // 3b. Two-block structures: beds, doors, and double plants.
    let path = block.name.strip_prefix("minecraft:").unwrap_or(&block.name);
    if path.ends_with("_bed") {
        let facing = horizontal_facing(ctx.yaw);
        let offset = direction_offset(facing);
        let mut foot_props = BTreeMap::new();
        foot_props.insert("facing".into(), facing.into());
        foot_props.insert("part".into(), "foot".into());
        let mut head_props = BTreeMap::new();
        head_props.insert("facing".into(), facing.into());
        head_props.insert("part".into(), "head".into());
        if let (Some(primary), Some(secondary)) = (
            reg.resolve_state(&block.name, &foot_props),
            reg.resolve_state(&block.name, &head_props),
        ) {
            return Some(PlacementResult::PlacePair {
                primary,
                secondary_offset: offset,
                secondary,
            });
        }
    }
    if path.ends_with("_door") {
        let facing = horizontal_facing(ctx.yaw);
        let mut lower_props = BTreeMap::new();
        lower_props.insert("facing".into(), facing.into());
        lower_props.insert("half".into(), "lower".into());
        lower_props.insert("open".into(), "false".into());
        let mut upper_props = BTreeMap::new();
        upper_props.insert("facing".into(), facing.into());
        upper_props.insert("half".into(), "upper".into());
        upper_props.insert("open".into(), "false".into());
        if let (Some(primary), Some(secondary)) = (
            reg.resolve_state(&block.name, &lower_props),
            reg.resolve_state(&block.name, &upper_props),
        ) {
            return Some(PlacementResult::PlacePair {
                primary,
                secondary_offset: [0, 1, 0],
                secondary,
            });
        }
    }
    if is_double_plant(path) {
        let mut lower_props = BTreeMap::new();
        lower_props.insert("half".into(), "lower".into());
        let mut upper_props = BTreeMap::new();
        upper_props.insert("half".into(), "upper".into());
        if let (Some(primary), Some(secondary)) = (
            reg.resolve_state(&block.name, &lower_props),
            reg.resolve_state(&block.name, &upper_props),
        ) {
            return Some(PlacementResult::PlacePair {
                primary,
                secondary_offset: [0, 1, 0],
                secondary,
            });
        }
    }

    // 4. General property resolution based on the block's declared properties.
    let mut props = BTreeMap::new();

    if block.properties.contains_key("waterlogged") && is_water_source(ctx.target_state) {
        props.insert("waterlogged".into(), "true".into());
    }

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

fn direction_offset(dir: &str) -> [i32; 3] {
    match dir {
        "north" => [0, 0, -1],
        "south" => [0, 0, 1],
        "west" => [-1, 0, 0],
        "east" => [1, 0, 0],
        "up" => [0, 1, 0],
        "down" => [0, -1, 0],
        _ => [0, 0, 0],
    }
}

fn is_double_plant(path: &str) -> bool {
    matches!(
        path,
        "tall_grass"
            | "large_fern"
            | "sunflower"
            | "lilac"
            | "rose_bush"
            | "peony"
            | "pitcher_plant"
    )
}

fn is_water_source(state: i32) -> bool {
    registry().state_by_id(state).is_some_and(|(_, def, sdef)| {
        def.name == "minecraft:water"
            && sdef.properties.get("level").map(String::as_str) == Some("0")
    })
}

/// Returns `true` if `state` is a solid non-replaceable support block.
pub fn is_solid_support(state: i32) -> bool {
    crate::game::block_by_state(state).is_some_and(|d| d.collision && !d.replaceable)
}

/// If a block with `state` requires an adjacent solid support block, returns
/// the offset `[dx, dy, dz]` to that support block.
pub fn required_support_offset(state: i32) -> Option<[i32; 3]> {
    let (_, def, sdef) = registry().state_by_id(state)?;
    let path = def.name.strip_prefix("minecraft:").unwrap_or(&def.name);
    if path.ends_with("_wall_torch")
        || path.ends_with("_wall_sign")
        || path.ends_with("_wall_banner")
        || matches!(path, "ladder" | "tripwire_hook")
    {
        let facing = sdef.properties.get("facing").map(String::as_str)?;
        return Some(direction_offset(opposite_facing(facing)));
    }
    if path.ends_with("_button") || path == "lever" {
        return match sdef.properties.get("face").map(String::as_str) {
            Some("floor") => Some([0, -1, 0]),
            Some("ceiling") => Some([0, 1, 0]),
            Some("wall") => {
                let facing = sdef.properties.get("facing").map(String::as_str)?;
                Some(direction_offset(opposite_facing(facing)))
            }
            _ => Some([0, -1, 0]),
        };
    }
    if matches!(
        path,
        "torch"
            | "soul_torch"
            | "redstone_torch"
            | "redstone_wire"
            | "repeater"
            | "comparator"
            | "rail"
            | "powered_rail"
            | "detector_rail"
            | "activator_rail"
            | "flower_pot"
            | "snow"
            | "short_grass"
            | "fern"
    ) || path.ends_with("_carpet")
        || path.ends_with("_pressure_plate")
        || path.ends_with("_sapling")
        || (path.ends_with("_sign") && !path.contains("hanging") && !path.contains("wall"))
    {
        return Some([0, -1, 0]);
    }
    None
}

/// If `state` is half of a two-block structure (bed, door, double plant),
/// returns the world position of its paired half.
pub fn paired_block_pos(pos: BlockPos, state: i32) -> Option<BlockPos> {
    let (_, def, sdef) = registry().state_by_id(state)?;
    let path = def.name.strip_prefix("minecraft:").unwrap_or(&def.name);
    if path.ends_with("_bed") {
        let facing = sdef.properties.get("facing").map(String::as_str)?;
        let part = sdef.properties.get("part").map(String::as_str)?;
        let dir = if part == "foot" {
            facing
        } else {
            opposite_facing(facing)
        };
        let [dx, dy, dz] = direction_offset(dir);
        return Some(BlockPos([pos.0[0] + dx, pos.0[1] + dy, pos.0[2] + dz]));
    }
    if path.ends_with("_door") || is_double_plant(path) {
        let half = sdef.properties.get("half").map(String::as_str)?;
        let dy = if half == "lower" { 1 } else { -1 };
        return Some(BlockPos([pos.0[0], pos.0[1] + dy, pos.0[2]]));
    }
    None
}

fn rotate_ccw(dir: &str) -> &'static str {
    match dir {
        "north" => "west",
        "west" => "south",
        "south" => "east",
        "east" => "north",
        _ => "north",
    }
}

/// Compute vanilla stair corner `shape` (`straight`, `inner_left`, `inner_right`,
/// `outer_left`, `outer_right`) given neighboring block states.
pub fn resolve_stairs_shape(pos: BlockPos, state: i32, block_at: impl Fn(BlockPos) -> i32) -> i32 {
    let reg = registry();
    let Some((_, def, sdef)) = reg.state_by_id(state) else {
        return state;
    };
    if !def.name.ends_with("_stairs") {
        return state;
    }
    let Some(facing) = sdef.properties.get("facing").map(String::as_str) else {
        return state;
    };
    let Some(half) = sdef.properties.get("half").map(String::as_str) else {
        return state;
    };
    let stair_at = |dir: &str| -> Option<(&str, &str)> {
        let [dx, dy, dz] = direction_offset(dir);
        let nstate = block_at(BlockPos([pos.0[0] + dx, pos.0[1] + dy, pos.0[2] + dz]));
        let (_, ndef, nsdef) = reg.state_by_id(nstate)?;
        if !ndef.name.ends_with("_stairs") {
            return None;
        }
        Some((
            nsdef.properties.get("facing").map(String::as_str)?,
            nsdef.properties.get("half").map(String::as_str)?,
        ))
    };

    let mut shape = "straight";
    if let Some((nf, nh)) = stair_at(facing)
        && nh == half
        && nf != facing
        && nf != opposite_facing(facing)
    {
        shape = if nf == rotate_ccw(facing) {
            "outer_left"
        } else {
            "outer_right"
        };
    } else if let Some((nf, nh)) = stair_at(opposite_facing(facing))
        && nh == half
        && nf != facing
        && nf != opposite_facing(facing)
    {
        shape = if nf == rotate_ccw(facing) {
            "inner_left"
        } else {
            "inner_right"
        };
    }

    if sdef.properties.get("shape").map(String::as_str) == Some(shape) {
        return state;
    }
    let mut props: BTreeMap<String, String> = sdef
        .properties
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    props.insert("shape".into(), shape.into());
    reg.resolve_state(&def.name, &props).unwrap_or(state)
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
