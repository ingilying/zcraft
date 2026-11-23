use super::{take, take_char};
use crate::command::{Anchor, CommandError, StringReader};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceLocation {
    pub namespace: String,
    pub path: String,
}
impl fmt::Display for ResourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.namespace, self.path)
    }
}
impl ResourceLocation {
    pub fn parse(text: &str) -> Result<Self, CommandError> {
        let (namespace, path) = text.split_once(':').unwrap_or(("minecraft", text));
        let namespace = if namespace.is_empty() {
            "minecraft"
        } else {
            namespace
        };
        let allowed =
            |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-' | b'.');
        if !namespace.bytes().all(allowed) || !path.bytes().all(|c| allowed(c) || c == b'/') {
            return Err(CommandError::new(
                "Invalid resource location.",
                0..text.encode_utf16().count(),
            ));
        }
        Ok(Self {
            namespace: namespace.into(),
            path: path.into(),
        })
    }
}
pub(super) fn read_resource(
    reader: &mut StringReader<'_>,
) -> Result<ResourceLocation, CommandError> {
    let start = reader.cursor();
    let text = take(reader, |c| {
        c <= 127
            && ((c as u8).is_ascii_lowercase()
                || (c as u8).is_ascii_digit()
                || matches!(c as u8, b'_' | b'-' | b'.' | b'/' | b':'))
    });
    ResourceLocation::parse(&text).map_err(|_| {
        let end = reader.cursor();
        reader.set_cursor(start);
        CommandError::new("Invalid resource location.", start..end)
    })
}
pub(super) fn read_uuid(reader: &mut StringReader<'_>) -> Result<Uuid, CommandError> {
    let start = reader.cursor();
    let text = take(reader, |c| {
        c <= 127 && ((c as u8).is_ascii_hexdigit() || c == b'-' as u16)
    });
    let parts: Vec<_> = text.split('-').collect();
    let widths = [8, 4, 4, 4, 12];
    if parts.len() == 5
        && parts
            .iter()
            .zip(widths)
            .all(|(part, w)| !part.is_empty() && part.len() <= w)
    {
        let canonical = parts
            .iter()
            .zip(widths)
            .map(|(part, w)| format!("{part:0>w$}"))
            .collect::<Vec<_>>()
            .join("-");
        if let Ok(uuid) = Uuid::parse_str(&canonical) {
            return Ok(uuid);
        }
    }
    let end = reader.cursor();
    reader.set_cursor(start);
    Err(CommandError::new("Invalid UUID.", start..end))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntRange {
    pub min: Option<i32>,
    pub max: Option<i32>,
}
impl IntRange {
    pub fn contains(self, value: i32) -> bool {
        self.min.is_none_or(|v| value >= v) && self.max.is_none_or(|v| value <= v)
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatRange {
    pub min: Option<f64>,
    pub max: Option<f64>,
}
impl FloatRange {
    pub fn contains(self, value: f64) -> bool {
        value.is_finite()
            && self.min.is_none_or(|v| value >= v)
            && self.max.is_none_or(|v| value <= v)
    }
}
fn bound(reader: &mut StringReader<'_>, _float: bool) -> String {
    let start = reader.cursor();
    while let Some(c) = reader.peek() {
        if c == b'.' as u16 && reader.remaining().get(1) == Some(&(b'.' as u16)) {
            break;
        }
        if c <= 127 && ((c as u8).is_ascii_digit() || c == b'-' as u16 || c == b'.' as u16) {
            reader.read();
        } else {
            break;
        }
    }
    // Cursor is UTF-16; numeric tokens are ASCII.
    let end = reader.cursor();
    reader.set_cursor(start);
    let s = String::from_utf16_lossy(&reader.remaining()[..end - start]);
    reader.set_cursor(end);
    s
}
fn range<T: Copy + PartialOrd>(
    reader: &mut StringReader<'_>,
    float: bool,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<(Option<T>, Option<T>), CommandError> {
    let start = reader.cursor();
    let result = (|| {
        let text = bound(reader, float);
        let min = if text.is_empty() {
            None
        } else {
            Some(parse(&text).ok_or(())?)
        };
        let max = if reader.remaining().starts_with(&[46, 46]) {
            reader.read();
            reader.read();
            let text = bound(reader, float);
            if text.is_empty() {
                None
            } else {
                Some(parse(&text).ok_or(())?)
            }
        } else {
            min
        };
        if min.is_none() && max.is_none() || matches!((min,max),(Some(a),Some(b)) if a>b) {
            return Err(());
        }
        Ok((min, max))
    })();
    result.map_err(|()| {
        let end = reader.cursor();
        reader.set_cursor(start);
        CommandError::new("Invalid or empty range.", start..end)
    })
}
pub(super) fn read_int_range(r: &mut StringReader<'_>) -> Result<IntRange, CommandError> {
    let (min, max) = range(r, false, |s| s.parse().ok())?;
    Ok(IntRange { min, max })
}
pub(super) fn read_float_range(r: &mut StringReader<'_>) -> Result<FloatRange, CommandError> {
    let (min, max) = range(r, true, |s| s.parse::<f64>().ok().filter(|n| n.is_finite()))?;
    Ok(FloatRange { min, max })
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Axes(pub u8);
impl Axes {
    pub fn x(self) -> bool {
        self.0 & 1 != 0
    }
    pub fn y(self) -> bool {
        self.0 & 2 != 0
    }
    pub fn z(self) -> bool {
        self.0 & 4 != 0
    }
}
pub(super) fn read_axes(r: &mut StringReader<'_>) -> Result<Axes, CommandError> {
    let mut bits = 0;
    while let Some(c) = r.peek().filter(|c| *c != 32) {
        r.read();
        let b = match c {
            120 => 1,
            121 => 2,
            122 => 4,
            _ => return Err(r.error("Invalid axis.")),
        };
        if bits & b != 0 {
            return Err(r.error("Duplicate axis."));
        }
        bits |= b;
    }
    Ok(Axes(bits))
}
pub(super) fn read_anchor(r: &mut StringReader<'_>) -> Result<Anchor, CommandError> {
    let start = r.cursor();
    let s = r.read_word();
    match s.as_str() {
        "feet" => Ok(Anchor::Feet),
        "eyes" => Ok(Anchor::Eyes),
        _ => {
            r.set_cursor(start);
            Err(r.error("Invalid entity anchor."))
        }
    }
}

pub(super) fn read_number(r: &mut StringReader<'_>, integer: bool) -> Result<f64, CommandError> {
    let start = r.cursor();
    let s = take(r, |c| {
        c <= 127 && ((c as u8).is_ascii_digit() || matches!(c, 45 | 46))
    });
    let number = if integer {
        s.parse::<i32>().ok().map(f64::from)
    } else {
        s.parse::<f64>().ok().filter(|v| v.is_finite())
    };
    number.ok_or_else(|| {
        let end = r.cursor();
        r.set_cursor(start);
        CommandError::new("Expected a finite number.", start..end)
    })
}
pub(super) fn relative(r: &mut StringReader<'_>) -> bool {
    take_char(r, b'~')
}
