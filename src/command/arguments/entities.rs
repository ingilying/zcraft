//! Selector syntax and evaluation against owned snapshots of currently available entities.
use super::{FloatRange, ResourceLocation, scalar};
use crate::command::{
    ArgumentKind, ArgumentParser, ArgumentValue, CommandError, CommandNode, SourceStack,
    StringReader, Suggestion, SuggestionContext,
};
use rand::{Rng, seq::SliceRandom};
use std::{collections::HashSet, fmt, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityGameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
}
impl EntityGameMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "survival" => Some(Self::Survival),
            "creative" => Some(Self::Creative),
            "adventure" => Some(Self::Adventure),
            "spectator" => Some(Self::Spectator),
            _ => None,
        }
    }
}
/// Owned query data: no live slot references or locks cross evaluator calls.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySnapshot {
    pub uuid: Uuid,
    pub name: String,
    pub entity_type: ResourceLocation,
    pub world: String,
    pub position: [f64; 3],
    pub rotation: [f32; 2],
    pub width: f64,
    pub height: f64,
    pub game_mode: Option<EntityGameMode>,
}
impl EntitySnapshot {
    pub fn is_player(&self) -> bool {
        self.entity_type.namespace == "minecraft" && self.entity_type.path == "player"
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityArgument {
    pub single: bool,
    pub players_only: bool,
}
impl EntityArgument {
    pub const fn new(single: bool, players_only: bool) -> Self {
        Self {
            single,
            players_only,
        }
    }
    pub fn kind(self) -> ArgumentKind {
        ArgumentKind::Custom(Arc::new(self))
    }
    pub fn node(self, name: impl Into<String>) -> CommandNode {
        CommandNode::argument(name, self.kind()).suggests(suggestions)
    }
}
impl ArgumentParser for EntityArgument {
    fn parse(&self, r: &mut StringReader<'_>) -> Result<ArgumentValue, CommandError> {
        let start = r.cursor();
        let query = parse_query(r, *self)?;
        if self.single && query.max_results() > 1 {
            r.set_cursor(start);
            return Err(r.error("Only one entity is allowed."));
        }
        if self.players_only
            && matches!(&query.target,Target::Selector(s) if s.kind=='e' && !s.player_type_required())
        {
            r.set_cursor(start);
            return Err(r.error("Only players are allowed."));
        }
        Ok(ArgumentValue::Custom(Arc::new(query)))
    }
    fn examples(&self) -> &[&str] {
        &["Player", "@p", "@e[type=player,limit=1]"]
    }
    fn wire(&self) -> Option<(i32, Vec<u8>)> {
        Some((
            6,
            vec![u8::from(self.single) | u8::from(self.players_only) << 1],
        ))
    }
}
fn suggestions(ctx: &SuggestionContext<'_>) -> Vec<Suggestion> {
    if ctx.source.permission < 2 {
        return vec![];
    }
    let remaining = ctx.remaining();
    ["@p", "@a", "@r", "@s", "@e"]
        .into_iter()
        .filter(|s| s.starts_with(&remaining) && *s != remaining)
        .map(|s| ctx.suggest(s))
        .collect()
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorSort {
    Arbitrary,
    Nearest,
    Furthest,
    Random,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Name(String),
    Uuid(Uuid),
    Selector(Box<Selector>),
}
#[derive(Debug, Clone, PartialEq)]
pub struct EntityQuery {
    pub target: Target,
    argument: EntityArgument,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    pub kind: char,
    pub limit: usize,
    pub sort: SelectorSort,
    pub origin: [Option<f64>; 3],
    pub delta: [Option<f64>; 3],
    pub distance: Option<FloatRange>,
    pub names: Vec<(bool, String)>,
    pub types: Vec<(bool, ResourceLocation)>,
    pub game_modes: Vec<(bool, EntityGameMode)>,
    pub rotation: [Option<FloatRange>; 2],
}
impl Selector {
    fn player_type_required(&self) -> bool {
        self.types
            .iter()
            .any(|(not, t)| !not && t.namespace == "minecraft" && t.path == "player")
    }
    fn limited_world(&self) -> bool {
        self.origin.iter().any(Option::is_some)
            || self.delta.iter().any(Option::is_some)
            || self.distance.is_some()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityQueryError {
    Permission,
    MissingPosition,
    MissingDimension,
    NoEntities,
    TooManyEntities,
    InvalidSnapshot,
}
impl fmt::Display for EntityQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Permission => "Selectors require permission level 2.",
            Self::MissingPosition => "Selector requires a source position.",
            Self::MissingDimension => "Selector requires a source dimension.",
            Self::NoEntities => "No matching entities.",
            Self::TooManyEntities => "Expected one entity.",
            Self::InvalidSnapshot => "Invalid entity snapshot.",
        })
    }
}
impl std::error::Error for EntityQueryError {}
impl EntityQuery {
    fn max_results(&self) -> usize {
        match &self.target {
            Target::Selector(s) => s.limit,
            _ => 1,
        }
    }
    /// Random selection is explicitly injected, allowing repeatable tests and a
    /// server RNG without ever pretending the first result is random.
    pub fn resolve<'a, R: Rng + ?Sized>(
        &self,
        source: &SourceStack,
        entities: &'a [EntitySnapshot],
        rng: &mut R,
    ) -> Result<Vec<&'a EntitySnapshot>, EntityQueryError> {
        if entities.iter().any(|e| {
            !e.position.iter().all(|v| v.is_finite())
                || !e.rotation.iter().all(|v| v.is_finite())
                || !e.width.is_finite()
                || e.width < 0.0
                || !e.height.is_finite()
                || e.height < 0.0
        }) {
            return Err(EntityQueryError::InvalidSnapshot);
        }
        let mut result: Vec<_> = match &self.target {
            Target::Name(name) => entities
                .iter()
                .filter(|e| e.is_player() && e.name == *name)
                .collect(),
            Target::Uuid(uuid) => entities.iter().filter(|e| e.uuid == *uuid).collect(),
            Target::Selector(s) => {
                if !source.has_permission(2) {
                    return Err(EntityQueryError::Permission);
                }
                let need_position = s.limited_world()
                    || matches!(s.sort, SelectorSort::Nearest | SelectorSort::Furthest);
                let base = source.position();
                let origin = [0, 1, 2].map(|i| s.origin[i].or(base.map(|p| p[i])));
                let origin = if need_position {
                    [
                        origin[0].ok_or(EntityQueryError::MissingPosition)?,
                        origin[1].ok_or(EntityQueryError::MissingPosition)?,
                        origin[2].ok_or(EntityQueryError::MissingPosition)?,
                    ]
                } else {
                    [0.0; 3]
                };
                let world = source.source().origin.as_ref().map(|p| p.world.as_str());
                if s.limited_world() && world.is_none() {
                    return Err(EntityQueryError::MissingDimension);
                }
                let dist = |e: &EntitySnapshot| {
                    e.position
                        .iter()
                        .zip(origin)
                        .map(|(a, b)| (a - b).powi(2))
                        .sum::<f64>()
                };
                let mut matches: Vec<_> = entities
                    .iter()
                    .filter(|e| {
                        if s.kind == 's' && source.entity().map(|v| v.uuid) != Some(e.uuid) {
                            return false;
                        }
                        if matches!(s.kind, 'p' | 'a' | 'r') && !e.is_player() {
                            return false;
                        }
                        if s.limited_world() && Some(e.world.as_str()) != world {
                            return false;
                        }
                        if !s.names.iter().all(|(not, n)| (e.name == *n) != *not)
                            || !s.types.iter().all(|(not, t)| (e.entity_type == *t) != *not)
                            || !s
                                .game_modes
                                .iter()
                                .all(|(not, g)| e.game_mode.is_some_and(|m| (m == *g) != *not))
                        {
                            return false;
                        }
                        if s.distance
                            .is_some_and(|range| !range.contains(dist(e).sqrt()))
                        {
                            return false;
                        }
                        if s.delta.iter().any(Option::is_some) {
                            for (i, base) in origin.iter().enumerate() {
                                let d = s.delta[i].unwrap_or(0.0);
                                let min = base + d.min(0.0);
                                let max = base + d.max(0.0) + 1.0;
                                let (a, b) = if i == 1 {
                                    (e.position[i], e.position[i] + e.height)
                                } else {
                                    (e.position[i] - e.width / 2.0, e.position[i] + e.width / 2.0)
                                };
                                if b <= min || a >= max {
                                    return false;
                                }
                            }
                        }
                        for (i, range) in s.rotation.iter().enumerate() {
                            if let Some(range) = range {
                                let angle =
                                    (f64::from(e.rotation[i]) + 180.0).rem_euclid(360.0) - 180.0;
                                let min = range.min.map(|v| (v + 180.0).rem_euclid(360.0) - 180.0);
                                let max = range.max.map(|v| (v + 180.0).rem_euclid(360.0) - 180.0);
                                let valid = match (min, max) {
                                    (Some(a), Some(b)) if a > b => angle >= a || angle <= b,
                                    _ => {
                                        min.is_none_or(|v| angle >= v)
                                            && max.is_none_or(|v| angle <= v)
                                    }
                                };
                                if !valid {
                                    return false;
                                }
                            }
                        }
                        true
                    })
                    .collect();
                match s.sort {
                    SelectorSort::Arbitrary => {}
                    SelectorSort::Random => matches.shuffle(rng),
                    SelectorSort::Nearest => matches.sort_by(|a, b| dist(a).total_cmp(&dist(b))),
                    SelectorSort::Furthest => matches.sort_by(|a, b| dist(b).total_cmp(&dist(a))),
                }
                matches.truncate(s.limit);
                matches
            }
        };
        if self.argument.players_only {
            result.retain(|e| e.is_player());
        }
        if self.argument.single {
            if result.is_empty() {
                return Err(EntityQueryError::NoEntities);
            }
            if result.len() > 1 {
                return Err(EntityQueryError::TooManyEntities);
            }
        }
        Ok(result)
    }
}

fn space(r: &mut StringReader<'_>) {
    while r.peek() == Some(32) {
        r.read();
    }
}
fn expect(r: &mut StringReader<'_>, ch: u8) -> Result<(), CommandError> {
    if r.peek() == Some(ch.into()) {
        r.read();
        Ok(())
    } else {
        Err(r.error(format!("Expected '{}'.", ch as char)))
    }
}
fn value(r: &mut StringReader<'_>) -> Result<String, CommandError> {
    if matches!(r.peek(), Some(34 | 39)) {
        r.read_string()
    } else {
        let mut v = Vec::new();
        while let Some(c) = r.peek().filter(|c| !matches!(*c, 32 | 44 | 93)) {
            v.push(c);
            r.read();
        }
        Ok(String::from_utf16_lossy(&v))
    }
}
fn finite(s: &str, r: &StringReader<'_>) -> Result<f64, CommandError> {
    let units: Vec<_> = s.encode_utf16().collect();
    let mut value = StringReader::new(&units);
    let number = scalar::read_number(&mut value, false)
        .map_err(|_| r.error("Expected finite selector number."))?;
    if value.peek().is_some() {
        return Err(r.error("Expected finite selector number."));
    }
    Ok(number)
}
fn parse_range(s: &str, r: &StringReader<'_>, rotation: bool) -> Result<FloatRange, CommandError> {
    if rotation {
        let parts: Vec<_> = s.split("..").collect();
        let result = match parts.as_slice() {
            [one] if !one.is_empty() => {
                let n = finite(one, r)?;
                FloatRange {
                    min: Some(n),
                    max: Some(n),
                }
            }
            [a, b] if !a.is_empty() || !b.is_empty() => FloatRange {
                min: if a.is_empty() {
                    None
                } else {
                    Some(finite(a, r)?)
                },
                max: if b.is_empty() {
                    None
                } else {
                    Some(finite(b, r)?)
                },
            },
            _ => return Err(r.error("Invalid rotation range.")),
        };
        return Ok(result);
    }
    let units: Vec<_> = s.encode_utf16().collect();
    let mut rr = StringReader::new(&units);
    let v = scalar::read_float_range(&mut rr).map_err(|_| r.error("Invalid distance range."))?;
    if rr.peek().is_some() || v.min.is_some_and(|n| n < 0.0) || v.max.is_some_and(|n| n < 0.0) {
        return Err(r.error("Distance must be nonnegative."));
    }
    Ok(v)
}
fn parse_query(
    r: &mut StringReader<'_>,
    argument: EntityArgument,
) -> Result<EntityQuery, CommandError> {
    let start = r.cursor();
    if r.remaining().len() > 4096 {
        return Err(r.error("Selector input exceeds limit."));
    }
    let target = if r.peek() != Some(64) {
        let name = r.read_string()?;
        if name.is_empty() {
            return Err(r.error("Expected player name or UUID."));
        }
        if let Ok(uuid) = Uuid::parse_str(&name) {
            Target::Uuid(uuid)
        } else if crate::auth::valid_username(&name) {
            Target::Name(name)
        } else {
            r.set_cursor(start);
            return Err(r.error("Invalid player name or UUID."));
        }
    } else {
        r.read();
        let kind = r
            .read()
            .and_then(|c| char::from_u32(c.into()))
            .ok_or_else(|| r.error("Missing selector kind."))?;
        if !matches!(kind, 'p' | 'a' | 'r' | 's' | 'e') {
            return Err(r.error("Unknown selector."));
        }
        let mut s = Selector {
            kind,
            limit: if matches!(kind, 'p' | 'r' | 's') {
                1
            } else {
                usize::MAX
            },
            sort: match kind {
                'p' => SelectorSort::Nearest,
                'r' => SelectorSort::Random,
                _ => SelectorSort::Arbitrary,
            },
            origin: [None; 3],
            delta: [None; 3],
            distance: None,
            names: vec![],
            types: vec![],
            game_modes: vec![],
            rotation: [None; 2],
        };
        if r.peek() == Some(91) {
            r.read();
            space(r);
            let mut seen = HashSet::new();
            let mut options = 0;
            while r.peek() != Some(93) {
                if options == 32 {
                    return Err(r.error("Too many selector options."));
                }
                options += 1;
                let key = r.read_word();
                space(r);
                expect(r, b'=')?;
                space(r);
                let not = if r.peek() == Some(33) {
                    r.read();
                    true
                } else {
                    false
                };
                let text = value(r)?;
                space(r);
                let repeatable = matches!(key.as_str(), "name" | "type" | "gamemode");
                if (!repeatable && !seen.insert(key.clone())) || (not && !repeatable) {
                    return Err(r.error("Duplicate or invalid selector option."));
                }
                match key.as_str() {
                    "x" | "y" | "z" => {
                        let i = match key.as_str() {
                            "x" => 0,
                            "y" => 1,
                            _ => 2,
                        };
                        s.origin[i] = Some(finite(&text, r)?);
                    }
                    "dx" | "dy" | "dz" => {
                        let i = match key.as_str() {
                            "dx" => 0,
                            "dy" => 1,
                            _ => 2,
                        };
                        s.delta[i] = Some(finite(&text, r)?);
                    }
                    "distance" => s.distance = Some(parse_range(&text, r, false)?),
                    "x_rotation" => s.rotation[1] = Some(parse_range(&text, r, true)?),
                    "y_rotation" => s.rotation[0] = Some(parse_range(&text, r, true)?),
                    "limit" => {
                        if kind == 's' {
                            return Err(r.error("Self selector does not accept limit."));
                        }
                        if !text.bytes().all(|c| c.is_ascii_digit()) {
                            return Err(r.error("Invalid selector limit."));
                        }
                        s.limit = text
                            .parse::<usize>()
                            .ok()
                            .filter(|n| *n > 0 && *n <= i32::MAX as usize)
                            .ok_or_else(|| r.error("Invalid selector limit."))?;
                    }
                    "sort" => {
                        if kind == 's' {
                            return Err(r.error("Self selector does not accept sort."));
                        }
                        s.sort = match text.as_str() {
                            "nearest" => SelectorSort::Nearest,
                            "furthest" => SelectorSort::Furthest,
                            "random" => SelectorSort::Random,
                            "arbitrary" => SelectorSort::Arbitrary,
                            _ => return Err(r.error("Unknown selector sort.")),
                        };
                    }
                    "name" => {
                        if s.names.iter().any(|(n, _)| !*n) || (!not && !s.names.is_empty()) {
                            return Err(r.error("Incompatible name filters."));
                        }
                        s.names.push((not, text));
                    }
                    "gamemode" => {
                        if s.game_modes.iter().any(|(n, _)| !*n)
                            || (!not && !s.game_modes.is_empty())
                        {
                            return Err(r.error("Incompatible gamemode filters."));
                        }
                        let mode = EntityGameMode::parse(&text)
                            .ok_or_else(|| r.error("Unknown game mode."))?;
                        s.game_modes.push((not, mode));
                    }
                    "type" => {
                        if s.types.iter().any(|(n, _)| !*n) || (!not && !s.types.is_empty()) {
                            return Err(r.error("Incompatible type filters."));
                        }
                        let ty = ResourceLocation::parse(&text)
                            .map_err(|_| r.error("Invalid entity type."))?;
                        if ty.to_string() != "minecraft:player" {
                            return Err(
                                r.error("Only the existing minecraft:player type is supported.")
                            );
                        }
                        s.types.push((not, ty));
                    }
                    _ => return Err(r.error(format!("Unsupported selector option: {key}"))),
                }
                if r.peek() == Some(44) {
                    r.read();
                    space(r);
                    if r.peek() == Some(93) {
                        break;
                    }
                } else if r.peek() != Some(93) {
                    return Err(r.error("Expected ',' or ']'."));
                }
            }
            expect(r, b']')?;
        }
        Target::Selector(Box::new(s))
    };
    Ok(EntityQuery { target, argument })
}
