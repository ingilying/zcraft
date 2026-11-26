//! Scheduled block physics engine: gravity blocks (sand, gravel, concrete powder,
//! anvils) and vanilla-aligned fluids (water & lava spread, draining, infinite
//! water sources, and water/lava obsidian/cobblestone/stone mixing).
use super::BlockPos;
use crate::game::{AIR_STATE, block_behavior, block_by_state, registry};
use std::collections::{BTreeMap, HashMap};

const MAX_SCHEDULED_TICKS: usize = 8_192;
const MAX_TICKS_PER_STEP: usize = 128;
const GRAVITY_DELAY: u64 = 2;
const WATER_DELAY: u64 = 5;
const LAVA_DELAY: u64 = 30;

const NEIGHBORS_6: [[i32; 3]; 6] = [
    [0, 1, 0],
    [0, -1, 0],
    [1, 0, 0],
    [-1, 0, 0],
    [0, 0, 1],
    [0, 0, -1],
];

const HORIZONTAL_4: [[i32; 3]; 4] = [[1, 0, 0], [-1, 0, 0], [0, 0, 1], [0, 0, -1]];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum TickKind {
    Gravity,
    Water,
    Lava,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FluidKind {
    Water,
    Lava,
}

/// Deduplicated scheduled tick queue for gravity blocks and fluids.
#[derive(Debug, Default)]
pub(crate) struct PhysicsScheduler {
    tick: u64,
    pending: HashMap<(BlockPos, TickKind), u64>,
}

impl PhysicsScheduler {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn schedule(&mut self, pos: BlockPos, kind: TickKind, delay: u64) {
        if !pos.valid() {
            return;
        }
        let due = self.tick.saturating_add(delay.max(1));
        if let Some(existing) = self.pending.get_mut(&(pos, kind)) {
            if due < *existing {
                *existing = due;
            }
            return;
        }
        if self.pending.len() >= MAX_SCHEDULED_TICKS {
            return;
        }
        self.pending.insert((pos, kind), due);
    }

    /// Inspect `pos` and its 6 neighbors (passed as pre-read `(pos, state)` pairs)
    /// and schedule any gravity or fluid ticks that apply.
    pub(crate) fn schedule_samples(&mut self, samples: &[(BlockPos, i32)]) {
        for &(pos, state) in samples {
            if !pos.valid() {
                continue;
            }
            if block_behavior(state).falls_with_gravity() {
                self.schedule(pos, TickKind::Gravity, GRAVITY_DELAY);
            }
            if let Some((kind, _)) = fluid_info(state) {
                match kind {
                    FluidKind::Water => self.schedule(pos, TickKind::Water, WATER_DELAY),
                    FluidKind::Lava => self.schedule(pos, TickKind::Lava, LAVA_DELAY),
                }
            }
        }
    }

    /// Advance the physics clock by 1 tick and pop up to `MAX_TICKS_PER_STEP` due entries.
    pub(crate) fn pop_due(&mut self) -> Vec<(BlockPos, TickKind)> {
        self.tick = self.tick.saturating_add(1);
        let now = self.tick;
        let mut due: Vec<(u64, BlockPos, TickKind)> = self
            .pending
            .iter()
            .filter_map(|(&(pos, kind), &t)| (t <= now).then_some((t, pos, kind)))
            .collect();
        if due.is_empty() {
            return Vec::new();
        }
        // Sort deterministically: earliest due tick first, then lowest Y first so
        // stacked gravity columns fall cleanly from bottom to top.
        due.sort_unstable_by_key(|&(t, pos, kind)| (t, pos.0[1], pos.0[0], pos.0[2], kind));
        due.truncate(MAX_TICKS_PER_STEP);
        for &(_, pos, kind) in &due {
            self.pending.remove(&(pos, kind));
        }
        due.into_iter().map(|(_, pos, kind)| (pos, kind)).collect()
    }
}

/// Evaluate a single due physics tick, returning block state edits to apply.
pub(crate) fn evaluate_tick(
    pos: BlockPos,
    kind: TickKind,
    block_at: impl Fn(BlockPos) -> i32,
) -> Vec<(BlockPos, i32)> {
    match kind {
        TickKind::Gravity => evaluate_gravity(pos, block_at),
        TickKind::Water => evaluate_fluid(pos, FluidKind::Water, block_at),
        TickKind::Lava => evaluate_fluid(pos, FluidKind::Lava, block_at),
    }
}

fn evaluate_gravity(pos: BlockPos, block_at: impl Fn(BlockPos) -> i32) -> Vec<(BlockPos, i32)> {
    let state = block_at(pos);
    if !block_behavior(state).falls_with_gravity() {
        return Vec::new();
    }
    // Concrete powder touching water solidifies into concrete.
    if let Some(solid_state) = concrete_powder_to_concrete(state) {
        let touches_water = NEIGHBORS_6.iter().any(|&[dx, dy, dz]| {
            let npos = BlockPos([pos.0[0] + dx, pos.0[1] + dy, pos.0[2] + dz]);
            npos.valid() && matches!(fluid_info(block_at(npos)), Some((FluidKind::Water, _)))
        });
        if touches_water {
            return vec![(pos, solid_state)];
        }
    }

    let below = BlockPos([pos.0[0], pos.0[1] - 1, pos.0[2]]);
    if !below.valid() {
        return vec![(pos, AIR_STATE)];
    }
    if can_fall_through(block_at(below)) {
        vec![(pos, AIR_STATE), (below, state)]
    } else {
        Vec::new()
    }
}

fn can_fall_through(state: i32) -> bool {
    block_by_state(state).is_some_and(|d| d.air || d.liquid || (!d.collision && d.replaceable))
}

fn concrete_powder_to_concrete(state: i32) -> Option<i32> {
    let reg = registry();
    let (_, def, _) = reg.state_by_id(state)?;
    let prefix = def.name.strip_suffix("_concrete_powder")?;
    let solid_name = format!("{prefix}_concrete");
    reg.block_by_name(&solid_name).map(|(_, b)| b.default_state)
}

/// Returns `Some((kind, level))` if `state` is water, lava, or a waterlogged block.
/// Waterlogged blocks act as a water source (`level = 0`) for outgoing flow.
pub(crate) fn fluid_info(state: i32) -> Option<(FluidKind, u8)> {
    let (_, def, sdef) = registry().state_by_id(state)?;
    match def.name.as_str() {
        "minecraft:water" => {
            let level = sdef
                .properties
                .get("level")
                .and_then(|v| v.parse::<u8>().ok())
                .unwrap_or(0);
            Some((FluidKind::Water, level))
        }
        "minecraft:lava" => {
            let level = sdef
                .properties
                .get("level")
                .and_then(|v| v.parse::<u8>().ok())
                .unwrap_or(0);
            Some((FluidKind::Lava, level))
        }
        _ => {
            if sdef.properties.get("waterlogged").map(String::as_str) == Some("true") {
                Some((FluidKind::Water, 0))
            } else {
                None
            }
        }
    }
}

fn is_pure_fluid(state: i32, kind: FluidKind) -> bool {
    let target = match kind {
        FluidKind::Water => "minecraft:water",
        FluidKind::Lava => "minecraft:lava",
    };
    registry()
        .state_by_id(state)
        .is_some_and(|(_, def, _)| def.name == target)
}

fn fluid_state(kind: FluidKind, level: u8) -> Option<i32> {
    let name = match kind {
        FluidKind::Water => "minecraft:water",
        FluidKind::Lava => "minecraft:lava",
    };
    let mut props = BTreeMap::new();
    props.insert("level".into(), level.min(15).to_string());
    registry().resolve_state(name, &props)
}

fn evaluate_fluid(
    pos: BlockPos,
    kind: FluidKind,
    block_at: impl Fn(BlockPos) -> i32,
) -> Vec<(BlockPos, i32)> {
    let state = block_at(pos);
    let Some((actual_kind, mut level)) = fluid_info(state) else {
        return Vec::new();
    };
    if actual_kind != kind {
        return Vec::new();
    }
    let is_waterlogged = !is_pure_fluid(state, kind);
    let step: u8 = match kind {
        FluidKind::Water => 1,
        FluidKind::Lava => 2, // Overworld lava drops 2 levels per block (max 3 blocks).
    };
    let mut edits = Vec::new();

    // 1. Lava + Water horizontal/above contact -> Obsidian (source lava) or Cobblestone (flowing lava).
    if kind == FluidKind::Lava {
        let touches_water = [[0, 1, 0], [1, 0, 0], [-1, 0, 0], [0, 0, 1], [0, 0, -1]]
            .iter()
            .any(|&[dx, dy, dz]| {
                let npos = BlockPos([pos.0[0] + dx, pos.0[1] + dy, pos.0[2] + dz]);
                npos.valid() && matches!(fluid_info(block_at(npos)), Some((FluidKind::Water, _)))
            });
        if touches_water {
            let result_name = if level == 0 {
                "obsidian"
            } else {
                "cobblestone"
            };
            if let Some((_, def)) = registry().block_by_name(result_name) {
                edits.push((pos, def.default_state));
                return edits;
            }
        }
    }

    // 2. Flowing (non-source) fluid recalculates its level or drains back to air.
    if !is_waterlogged && level > 0 {
        let above = BlockPos([pos.0[0], pos.0[1] + 1, pos.0[2]]);
        let above_same =
            above.valid() && matches!(fluid_info(block_at(above)), Some((k, _)) if k == kind);

        let mut min_neighbor: Option<u8> = None;
        let mut source_neighbors = 0u8;
        for [dx, dy, dz] in HORIZONTAL_4 {
            let npos = BlockPos([pos.0[0] + dx, pos.0[1] + dy, pos.0[2] + dz]);
            if !npos.valid() {
                continue;
            }
            if let Some((nk, nlevel)) = fluid_info(block_at(npos))
                && nk == kind
            {
                if nlevel == 0 {
                    source_neighbors += 1;
                }
                let effective = if nlevel >= 8 { 0 } else { nlevel };
                min_neighbor = Some(match min_neighbor {
                    Some(cur) => cur.min(effective),
                    None => effective,
                });
            }
        }

        let below = BlockPos([pos.0[0], pos.0[1] - 1, pos.0[2]]);
        let below_supports_infinite = below.valid() && {
            let bstate = block_at(below);
            block_by_state(bstate).is_some_and(|d| d.collision)
                || matches!(fluid_info(bstate), Some((FluidKind::Water, 0)))
        };

        let expected: Option<u8> =
            if kind == FluidKind::Water && source_neighbors >= 2 && below_supports_infinite {
                Some(0)
            } else if above_same {
                Some(8)
            } else if let Some(min_n) = min_neighbor {
                let next = min_n.saturating_add(step);
                (next <= 7).then_some(next)
            } else {
                None
            };

        match expected {
            None => {
                edits.push((pos, AIR_STATE));
                return edits;
            }
            Some(exp) if exp != level => {
                if let Some(new_state) = fluid_state(kind, exp) {
                    edits.push((pos, new_state));
                    level = exp;
                }
            }
            _ => {}
        }
    }

    // 3. Downward spread.
    let below = BlockPos([pos.0[0], pos.0[1] - 1, pos.0[2]]);
    let mut flowed_down = false;
    if below.valid() {
        let bstate = block_at(below);
        if kind == FluidKind::Lava
            && matches!(fluid_info(bstate), Some((FluidKind::Water, _)))
            && is_pure_fluid(bstate, FluidKind::Water)
        {
            if let Some((_, stone)) = registry().block_by_name("stone") {
                edits.push((below, stone.default_state));
            }
            flowed_down = true;
        } else if can_fluid_enter(bstate, kind) {
            let already_falling_or_source =
                matches!(fluid_info(bstate), Some((k, l)) if k == kind && (l == 0 || l == 8));
            if !already_falling_or_source && let Some(falling) = fluid_state(kind, 8) {
                edits.push((below, falling));
            }
            flowed_down = true;
        } else if matches!(fluid_info(bstate), Some((k, _)) if k == kind) {
            flowed_down = true;
        }
    }

    // 4. Horizontal spread (from source, or when resting on a solid/non-flowable block).
    if level == 0 || !flowed_down {
        let effective = if level >= 8 { 0 } else { level };
        let next_level = effective.saturating_add(step);
        if next_level <= 7
            && let Some(spread_state) = fluid_state(kind, next_level)
        {
            for [dx, dy, dz] in HORIZONTAL_4 {
                let npos = BlockPos([pos.0[0] + dx, pos.0[1] + dy, pos.0[2] + dz]);
                if !npos.valid() {
                    continue;
                }
                let nstate = block_at(npos);
                if !can_fluid_enter(nstate, kind) {
                    continue;
                }
                if let Some((nk, nlevel)) = fluid_info(nstate)
                    && nk == kind
                    && (nlevel == 0 || nlevel <= next_level)
                {
                    continue;
                }
                edits.push((npos, spread_state));
            }
        }
    }

    edits
}

fn can_fluid_enter(state: i32, kind: FluidKind) -> bool {
    if is_pure_fluid(state, kind) {
        return true;
    }
    block_by_state(state).is_some_and(|d| d.air || (d.replaceable && !d.liquid && !d.collision))
}

pub(crate) fn neighborhood_positions(pos: BlockPos) -> [BlockPos; 7] {
    [
        pos,
        BlockPos([pos.0[0], pos.0[1] + 1, pos.0[2]]),
        BlockPos([pos.0[0], pos.0[1] - 1, pos.0[2]]),
        BlockPos([pos.0[0] + 1, pos.0[1], pos.0[2]]),
        BlockPos([pos.0[0] - 1, pos.0[1], pos.0[2]]),
        BlockPos([pos.0[0], pos.0[1], pos.0[2] + 1]),
        BlockPos([pos.0[0], pos.0[1], pos.0[2] - 1]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::World;

    #[test]
    fn sand_falls_to_ground_and_concrete_powder_solidifies_in_water() {
        let world = World::new();
        let sand = crate::game::block_by_name("sand").unwrap().default_state;
        world.set_block(BlockPos([4, 67, 4]), sand);
        for _ in 0..10 {
            world.tick_physics();
        }
        assert_eq!(world.block_at(BlockPos([4, 67, 4])), AIR_STATE);
        assert_eq!(world.block_at(BlockPos([4, 64, 4])), sand);

        // Concrete powder touching water turns into solid concrete.
        let water = crate::game::block_by_name("water").unwrap().default_state;
        let powder = crate::game::block_by_name("red_concrete_powder")
            .unwrap()
            .default_state;
        let concrete = crate::game::block_by_name("red_concrete")
            .unwrap()
            .default_state;
        world.set_block(BlockPos([10, 64, 10]), water);
        world.set_block(BlockPos([11, 64, 10]), powder);
        for _ in 0..3 {
            world.tick_physics();
        }
        assert_eq!(world.block_at(BlockPos([11, 64, 10])), concrete);
    }

    #[test]
    fn water_spreads_seven_blocks_drains_and_forms_infinite_source() {
        let world = World::new();
        let water = crate::game::block_by_name("water").unwrap().default_state;
        let origin = BlockPos([0, 64, 0]);
        world.set_block(origin, water);
        for _ in 0..45 {
            world.tick_physics();
        }
        assert_eq!(
            fluid_info(world.block_at(BlockPos([7, 64, 0]))),
            Some((FluidKind::Water, 7))
        );
        assert_eq!(world.block_at(BlockPos([8, 64, 0])), AIR_STATE);

        // Removing the source drains flowing water back to air.
        world.set_block(origin, AIR_STATE);
        for _ in 0..60 {
            world.tick_physics();
        }
        assert_eq!(world.block_at(BlockPos([1, 64, 0])), AIR_STATE);
        assert_eq!(world.block_at(BlockPos([7, 64, 0])), AIR_STATE);

        // Two orthogonal water sources create an infinite water source between them.
        world.set_block(BlockPos([0, 64, 20]), water);
        world.set_block(BlockPos([2, 64, 20]), water);
        for _ in 0..15 {
            world.tick_physics();
        }
        assert_eq!(
            fluid_info(world.block_at(BlockPos([1, 64, 20]))),
            Some((FluidKind::Water, 0))
        );
    }

    #[test]
    fn lava_and_water_mix_into_obsidian_and_stone() {
        let world = World::new();
        let water = crate::game::block_by_name("water").unwrap().default_state;
        let lava = crate::game::block_by_name("lava").unwrap().default_state;
        let obsidian = crate::game::block_by_name("obsidian")
            .unwrap()
            .default_state;
        let stone = crate::game::block_by_name("stone").unwrap().default_state;

        // Horizontal contact: source lava adjacent to water -> obsidian.
        world.set_block(BlockPos([20, 64, 0]), lava);
        world.set_block(BlockPos([21, 64, 0]), water);
        for _ in 0..35 {
            world.tick_physics();
        }
        assert_eq!(world.block_at(BlockPos([20, 64, 0])), obsidian);

        // Vertical contact: lava flowing down onto water -> stone.
        world.set_block(BlockPos([30, 64, 0]), water);
        world.set_block(BlockPos([30, 65, 0]), lava);
        for _ in 0..35 {
            world.tick_physics();
        }
        assert_eq!(world.block_at(BlockPos([30, 64, 0])), stone);
    }
}
