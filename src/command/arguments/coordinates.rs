use super::{scalar, take_char};
use crate::command::{CommandError, SourceStack, StringReader};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinateError {
    MissingPosition,
    NonFinite,
    BlockOverflow,
}
impl fmt::Display for CoordinateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingPosition => "Command position is unavailable.",
            Self::NonFinite => "Coordinate is not finite.",
            Self::BlockOverflow => "Block coordinate overflows i32.",
        })
    }
}
impl std::error::Error for CoordinateError {}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisCoordinate {
    pub relative: bool,
    pub value: f64,
}
impl AxisCoordinate {
    fn resolve(self, base: Option<f64>) -> Result<f64, CoordinateError> {
        let n = if self.relative {
            base.ok_or(CoordinateError::MissingPosition)? + self.value
        } else {
            self.value
        };
        if n.is_finite() {
            Ok(n)
        } else {
            Err(CoordinateError::NonFinite)
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Coordinates3 {
    World([AxisCoordinate; 3]),
    Local([f64; 3]),
}
impl Coordinates3 {
    pub fn resolve(self, source: &SourceStack) -> Result<[f64; 3], CoordinateError> {
        match self {
            Self::World(a) => Ok([
                a[0].resolve(source.position().map(|p| p[0]))?,
                a[1].resolve(source.position().map(|p| p[1]))?,
                a[2].resolve(source.position().map(|p| p[2]))?,
            ]),
            Self::Local([left, up, forward]) => {
                let p = source
                    .anchored_position()
                    .ok_or(CoordinateError::MissingPosition)?;
                let [yaw, pitch] = source.rotation().map(|v| f64::from(v).to_radians());
                let (sy, cy) = (yaw.sin(), yaw.cos());
                let (sp, cp) = (pitch.sin(), pitch.cos());
                let v = [
                    p[0] + left * cy + up * sy * sp - forward * sy * cp,
                    p[1] + up * cp - forward * sp,
                    p[2] + left * sy - up * cy * sp + forward * cy * cp,
                ];
                if v.iter().all(|v| v.is_finite()) {
                    Ok(v)
                } else {
                    Err(CoordinateError::NonFinite)
                }
            }
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3(pub Coordinates3);
impl Vec3 {
    pub fn resolve(self, s: &SourceStack) -> Result<[f64; 3], CoordinateError> {
        self.0.resolve(s)
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockPos(pub Coordinates3);
fn block(n: f64) -> Result<i32, CoordinateError> {
    let n = n.floor();
    if n.is_finite() && n >= i32::MIN as f64 && n <= i32::MAX as f64 {
        Ok(n as i32)
    } else {
        Err(CoordinateError::BlockOverflow)
    }
}
impl BlockPos {
    pub fn resolve(self, s: &SourceStack) -> Result<[i32; 3], CoordinateError> {
        let p = self.0.resolve(s)?;
        Ok([block(p[0])?, block(p[1])?, block(p[2])?])
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2(pub [AxisCoordinate; 2]);
impl Vec2 {
    pub fn resolve(self, s: &SourceStack) -> Result<[f64; 2], CoordinateError> {
        Ok([
            self.0[0].resolve(s.position().map(|p| p[0]))?,
            self.0[1].resolve(s.position().map(|p| p[2]))?,
        ])
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnPos(pub [AxisCoordinate; 2]);
impl ColumnPos {
    pub fn resolve(self, s: &SourceStack) -> Result<[i32; 2], CoordinateError> {
        let p = Vec2(self.0).resolve(s)?;
        Ok([block(p[0])?, block(p[1])?])
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rotation(pub [AxisCoordinate; 2]);
impl Rotation {
    pub fn resolve(self, s: &SourceStack) -> Result<[f64; 2], CoordinateError> {
        let r = s.rotation();
        Ok([
            self.0[0].resolve(Some(r[0].into()))?,
            self.0[1].resolve(Some(r[1].into()))?,
        ])
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Angle(pub AxisCoordinate);
impl Angle {
    pub fn resolve(self, s: &SourceStack) -> Result<f64, CoordinateError> {
        let n = self.0.resolve(Some(s.rotation()[0].into()))?;
        Ok((n + 180.0).rem_euclid(360.0) - 180.0)
    }
}

pub(super) fn read_angle(r: &mut StringReader<'_>) -> Result<Angle, CommandError> {
    if r.peek().is_none() {
        return Err(r.error("Incomplete angle."));
    }
    let relative = scalar::relative(r);
    let value = if r.peek().is_none_or(|c| c == 32) {
        0.0
    } else {
        scalar::read_number(r, false)?
    };
    let value = value as f32;
    if !value.is_finite() {
        return Err(r.error("Invalid angle."));
    }
    Ok(Angle(AxisCoordinate {
        relative,
        value: value.into(),
    }))
}
fn axis(
    r: &mut StringReader<'_>,
    center: bool,
    integer: bool,
) -> Result<AxisCoordinate, CommandError> {
    if r.peek() == Some(94) {
        return Err(r.error("Cannot mix local and world coordinates."));
    }
    if r.peek().is_none() {
        return Err(r.error("Incomplete coordinates."));
    }
    let relative = scalar::relative(r);
    let start = r.cursor();
    let value = if r.peek().is_none_or(|c| c == 32) {
        0.0
    } else {
        scalar::read_number(r, integer && !relative)?
    };
    let end = r.cursor();
    r.set_cursor(start);
    let decimal = r.remaining()[..end - start].contains(&46);
    r.set_cursor(end);
    Ok(AxisCoordinate {
        relative,
        value: if center && !relative && !decimal {
            value + 0.5
        } else {
            value
        },
    })
}
pub(super) fn read_world<const N: usize>(
    r: &mut StringReader<'_>,
    center: [bool; N],
    integer: bool,
) -> Result<[AxisCoordinate; N], CommandError> {
    let start = r.cursor();
    let mut axes = [AxisCoordinate {
        relative: false,
        value: 0.0,
    }; N];
    for i in 0..N {
        if i > 0 && (!take_char(r, b' ') || r.peek().is_none()) {
            r.set_cursor(start);
            return Err(r.error("Incomplete coordinates."));
        }
        axes[i] = axis(r, center[i], integer)?;
    }
    Ok(axes)
}
pub(super) fn read_3d(
    r: &mut StringReader<'_>,
    center: bool,
    integer: bool,
) -> Result<Coordinates3, CommandError> {
    if r.peek() != Some(94) {
        return read_world(r, [center, false, center], integer).map(Coordinates3::World);
    }
    let start = r.cursor();
    let mut values = [0.0; 3];
    for (i, value) in values.iter_mut().enumerate() {
        if i > 0 && (!take_char(r, b' ') || r.peek().is_none()) {
            r.set_cursor(start);
            return Err(r.error("Incomplete coordinates."));
        }
        if !take_char(r, b'^') {
            r.set_cursor(start);
            return Err(r.error("Cannot mix local and world coordinates."));
        }
        *value = if r.peek().is_none_or(|c| c == 32) {
            0.0
        } else {
            scalar::read_number(r, false)?
        };
    }
    Ok(Coordinates3::Local(values))
}
