//! Trusted Minecraft execution context, separate from the Brigadier input source.
use super::{CommandSource, Origin, SourceKind};
use std::{fmt, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct Dimension {
    pub id: String,
    /// Relative to overworld coordinates (for example, 8.0 for the Nether).
    pub coordinate_scale: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecutingEntity {
    pub uuid: Uuid,
    pub eye_height: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Anchor {
    #[default]
    Feet,
    Eyes,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FeedbackDestination {
    /// Return feedback to the caller, tagged with the original source.
    #[default]
    Caller,
    Discard,
}

pub type CommandCallback = Arc<dyn Fn(bool, i32) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    InvalidPosition,
    InvalidRotation,
    InvalidDimension,
    InvalidEyeHeight,
}
impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPosition => "Invalid command position.",
            Self::InvalidRotation => "Invalid command rotation.",
            Self::InvalidDimension => "Invalid command dimension metadata.",
            Self::InvalidEyeHeight => "Invalid command entity eye height.",
        })
    }
}
impl std::error::Error for SourceError {}

/// Clone-on-transform context. `source().kind` is the original, trusted
/// identity, never changed by execute-as. `entity()` is the current executor.
#[derive(Clone)]
pub struct SourceStack {
    source: CommandSource,
    ceiling: u8,
    dimension: Option<Dimension>,
    rotation: [f32; 2],
    entity: Option<ExecutingEntity>,
    anchor: Anchor,
    destination: FeedbackDestination,
    silent: bool,
    callback: Option<CommandCallback>,
}

impl SourceStack {
    pub fn new(source: CommandSource) -> Self {
        Self {
            ceiling: source.permission,
            source,
            dimension: None,
            rotation: [0.0; 2],
            entity: None,
            anchor: Anchor::Feet,
            destination: FeedbackDestination::Caller,
            silent: false,
            callback: None,
        }
    }
    pub fn source(&self) -> &CommandSource {
        &self.source
    }
    pub fn authenticated_player(&self) -> Option<Uuid> {
        match self.source.kind {
            SourceKind::Player(id) => Some(id),
            _ => None,
        }
    }
    pub fn has_permission(&self, level: u8) -> bool {
        self.source.permission >= level
    }
    pub fn dimension(&self) -> Option<&Dimension> {
        self.dimension.as_ref()
    }
    pub fn position(&self) -> Option<[f64; 3]> {
        self.source.origin.as_ref().map(|origin| origin.position)
    }
    /// [yaw, pitch], in degrees.
    pub fn rotation(&self) -> [f32; 2] {
        self.rotation
    }
    pub fn entity(&self) -> Option<ExecutingEntity> {
        self.entity
    }
    pub fn anchor(&self) -> Anchor {
        self.anchor
    }
    pub fn anchored_position(&self) -> Option<[f64; 3]> {
        let mut position = self.position()?;
        if self.anchor == Anchor::Eyes
            && let Some(entity) = self.entity
        {
            position[1] += entity.eye_height;
        }
        position
            .iter()
            .all(|value| value.is_finite())
            .then_some(position)
    }
    pub fn with_entity(mut self, entity: Option<ExecutingEntity>) -> Result<Self, SourceError> {
        if entity.is_some_and(|value| !value.eye_height.is_finite() || value.eye_height < 0.0) {
            return Err(SourceError::InvalidEyeHeight);
        }
        self.entity = entity;
        Ok(self)
    }
    pub fn with_anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }
    pub fn with_rotation(mut self, rotation: [f32; 2]) -> Result<Self, SourceError> {
        if !rotation.iter().all(|value| value.is_finite()) {
            return Err(SourceError::InvalidRotation);
        }
        self.rotation = rotation;
        Ok(self)
    }
    pub fn positioned(
        mut self,
        dimension: Dimension,
        position: [f64; 3],
    ) -> Result<Self, SourceError> {
        dimension.validate()?;
        if !position.iter().all(|value| value.is_finite()) {
            return Err(SourceError::InvalidPosition);
        }
        self.source.origin = Some(Origin {
            world: dimension.id.clone(),
            position,
        });
        self.dimension = Some(dimension);
        Ok(self)
    }
    pub fn with_position(mut self, position: [f64; 3]) -> Result<Self, SourceError> {
        if !position.iter().all(|value| value.is_finite()) {
            return Err(SourceError::InvalidPosition);
        }
        self.source
            .origin
            .as_mut()
            .ok_or(SourceError::InvalidDimension)?
            .position = position;
        Ok(self)
    }
    /// Scale X/Z by old_scale/new_scale, leaving Y unchanged. Callers must
    /// supply actual dimension metadata; an unscaled legacy origin is not enough.
    pub fn in_dimension(self, dimension: Dimension) -> Result<Self, SourceError> {
        dimension.validate()?;
        let previous = self
            .dimension
            .as_ref()
            .ok_or(SourceError::InvalidDimension)?;
        let mut position = self.position().ok_or(SourceError::InvalidPosition)?;
        let scale = previous.coordinate_scale / dimension.coordinate_scale;
        if !scale.is_finite() || scale == 0.0 {
            return Err(SourceError::InvalidDimension);
        }
        position[0] *= scale;
        position[2] *= scale;
        self.positioned(dimension, position)
    }
    pub fn with_permission(mut self, permission: u8) -> Self {
        self.source.permission = permission.min(self.ceiling);
        self
    }
    /// Like `withMaximumPermission`, raise to at least the requested level,
    /// but never above the ceiling assigned at the trusted entry point.
    pub fn with_maximum_permission(self, permission: u8) -> Self {
        let level = self.source.permission.max(permission);
        self.with_permission(level)
    }
    pub fn silent(mut self) -> Self {
        self.silent = true;
        self
    }
    pub fn with_feedback_destination(mut self, destination: FeedbackDestination) -> Self {
        self.destination = destination;
        self
    }
    pub fn feedback_destination(&self) -> FeedbackDestination {
        if self.silent {
            FeedbackDestination::Discard
        } else {
            self.destination
        }
    }
    pub fn with_callback(mut self, callback: CommandCallback) -> Self {
        self.callback = Some(match self.callback.take() {
            None => callback,
            Some(first) => Arc::new(move |success, value| {
                first(success, value);
                callback(success, value);
            }),
        });
        self
    }
    pub(super) fn complete(&self, success: bool, value: i32) {
        if let Some(callback) = &self.callback {
            callback(success, if success { value } else { 0 });
        }
    }
    pub(super) fn validate(&self) -> Result<(), SourceError> {
        if let Some(origin) = &self.source.origin {
            if origin.world.is_empty() || origin.world.len() > 256 {
                return Err(SourceError::InvalidDimension);
            }
            if !origin.position.iter().all(|value| value.is_finite()) {
                return Err(SourceError::InvalidPosition);
            }
        }
        Ok(())
    }
    /// Rebind a modifier's output to the invoker's authority and feedback
    /// policy. Runtime modifiers cannot substitute the originating sender.
    pub(super) fn inherit_authority(mut self, parent: &Self) -> Self {
        self.restrict_to(parent);
        self
    }
    pub(super) fn restrict_to(&mut self, parent: &Self) {
        self.source.kind = parent.source.kind.clone();
        self.ceiling = parent.source.permission;
        self.source.permission = self.source.permission.min(self.ceiling);
        self.silent |= parent.silent;
        if parent.feedback_destination() == FeedbackDestination::Discard {
            self.destination = FeedbackDestination::Discard;
        }
    }
    pub(super) fn legacy_transform(&self, source: CommandSource) -> Self {
        let mut result = self.clone();
        if result.dimension.as_ref().map(|dim| dim.id.as_str())
            != source.origin.as_ref().map(|origin| origin.world.as_str())
        {
            result.dimension = None;
        }
        result.source = source;
        result.inherit_authority(self)
    }
}

impl Dimension {
    fn validate(&self) -> Result<(), SourceError> {
        if self.id.is_empty()
            || self.id.len() > 256
            || !self.coordinate_scale.is_finite()
            || self.coordinate_scale <= 0.0
        {
            return Err(SourceError::InvalidDimension);
        }
        Ok(())
    }
}
