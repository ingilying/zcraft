//! StringReader semantics follow Brigadier 1.3.10 (see docs/third-party).
use super::ArgumentKind;
use std::{any::Any, collections::BTreeMap, fmt, ops::Range, sync::Arc};

#[derive(Debug, Clone)]
pub enum ArgumentValue {
    Text(String),
    Integer(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
    Custom(Arc<dyn Any + Send + Sync>),
}

#[derive(Debug, Default, Clone)]
pub struct Arguments(pub(super) BTreeMap<String, (ArgumentValue, Range<usize>)>);
impl Arguments {
    pub fn get(&self, name: &str) -> Option<&ArgumentValue> {
        self.0.get(name).map(|entry| &entry.0)
    }
    pub fn span(&self, name: &str) -> Option<Range<usize>> {
        self.0.get(name).map(|entry| entry.1.clone())
    }
    pub fn text(&self, name: &str) -> Option<&str> {
        if let ArgumentValue::Text(value) = self.get(name)? {
            Some(value)
        } else {
            None
        }
    }
    pub fn integer(&self, name: &str) -> Option<i32> {
        if let ArgumentValue::Integer(value) = self.get(name)? {
            Some(*value)
        } else {
            None
        }
    }
    pub fn boolean(&self, name: &str) -> Option<bool> {
        if let ArgumentValue::Bool(value) = self.get(name)? {
            Some(*value)
        } else {
            None
        }
    }
    pub fn custom<T: Any>(&self, name: &str) -> Option<&T> {
        if let ArgumentValue::Custom(value) = self.get(name)? {
            value.downcast_ref()
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub message: String,
    /// Java-compatible UTF-16 offsets into the original command.
    pub span: Range<usize>,
    pub usage: Option<String>,
}
impl CommandError {
    pub fn new(message: impl Into<String>, span: Range<usize>) -> Self {
        Self {
            message: message.into(),
            span,
            usage: None,
        }
    }
}
impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(usage) = &self.usage {
            write!(
                f,
                "{} (at {}); Usage: {}",
                self.message, self.span.start, usage
            )
        } else {
            f.write_str(&self.message)
        }
    }
}
impl std::error::Error for CommandError {}

/// UTF-16 cursor over a valid Unicode input. Substrings containing a split
/// surrogate are returned lossily because Rust strings cannot contain one.
#[derive(Clone)]
pub struct StringReader<'a> {
    units: &'a [u16],
    cursor: usize,
}
impl<'a> StringReader<'a> {
    pub fn new(units: &'a [u16]) -> Self {
        Self { units, cursor: 0 }
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.units.len());
    }
    pub fn remaining(&self) -> &'a [u16] {
        &self.units[self.cursor..]
    }
    pub fn peek(&self) -> Option<u16> {
        self.remaining().first().copied()
    }
    pub fn read(&mut self) -> Option<u16> {
        let unit = self.peek()?;
        self.cursor += 1;
        Some(unit)
    }
    pub fn error(&self, message: impl Into<String>) -> CommandError {
        CommandError::new(message, self.cursor..self.cursor)
    }
    fn take_while(&mut self, filter: impl Fn(u16) -> bool) -> String {
        let start = self.cursor;
        self.cursor += self
            .remaining()
            .iter()
            .copied()
            .take_while(|&u| filter(u))
            .count();
        String::from_utf16_lossy(&self.units[start..self.cursor])
    }
    pub fn read_word(&mut self) -> String {
        self.take_while(|unit| {
            unit <= 127 && ((unit as u8).is_ascii_alphanumeric() || b"_-.+".contains(&(unit as u8)))
        })
    }
    pub fn read_string(&mut self) -> Result<String, CommandError> {
        let Some(end @ (34 | 39)) = self.peek() else {
            return Ok(self.read_word());
        };
        self.read();
        let mut result = Vec::new();
        let mut escaped = false;
        while let Some(unit) = self.read() {
            if escaped {
                if unit != end && unit != 92 {
                    self.cursor -= 1;
                    return Err(self.error(format!(
                        "Invalid escape sequence '{}' in quoted string",
                        String::from_utf16_lossy(&[unit])
                    )));
                }
                result.push(unit);
                escaped = false;
            } else if unit == 92 {
                escaped = true;
            } else if unit == end {
                return Ok(String::from_utf16_lossy(&result));
            } else {
                result.push(unit);
            }
        }
        Err(self.error("Unclosed quoted string"))
    }
    pub fn read_remaining(&mut self) -> String {
        self.take_while(|_| true)
    }
    fn number<T: std::str::FromStr + PartialOrd + fmt::Display>(
        &mut self,
        kind: &str,
        min: T,
        max: T,
    ) -> Result<T, CommandError> {
        let start = self.cursor;
        let text = self.take_while(|u| matches!(u, 45 | 46 | 48..=57));
        if text.is_empty() {
            return Err(self.error(format!("Expected {kind}")));
        }
        let value = match text.parse::<T>() {
            Ok(value) => value,
            Err(_) => {
                self.cursor = start;
                return Err(self.error(format!("Invalid {kind} '{text}'")));
            }
        };
        if value < min || value > max {
            self.cursor = start;
            let label = match kind {
                "integer" => "Integer",
                "long" => "Long",
                "float" => "Float",
                _ => "Double",
            };
            let (comparison, bound) = if value < min {
                ("less", min)
            } else {
                ("more", max)
            };
            let display = |number: T| {
                let mut text = number.to_string();
                if matches!(kind, "float" | "double")
                    && number
                        .to_string()
                        .parse::<f64>()
                        .is_ok_and(|x| x.is_finite())
                    && !text.contains('.')
                {
                    text.push_str(".0");
                }
                text
            };
            return Err(self.error(format!(
                "{label} must not be {comparison} than {}, found {}",
                display(bound),
                display(value)
            )));
        }
        Ok(value)
    }
    pub(super) fn argument(&mut self, kind: &ArgumentKind) -> Result<ArgumentValue, CommandError> {
        Ok(match kind {
            ArgumentKind::Word => ArgumentValue::Text(self.read_word()),
            ArgumentKind::String => ArgumentValue::Text(self.read_string()?),
            ArgumentKind::Greedy => ArgumentValue::Text(self.read_remaining()),
            ArgumentKind::Bool => {
                let start = self.cursor;
                let value = self.read_string()?;
                ArgumentValue::Bool(match value.as_str() {
                    "true" => true,
                    "false" => false,
                    "" => return Err(self.error("Expected bool")),
                    _ => {
                        self.cursor = start;
                        return Err(self.error(format!(
                            "Invalid bool, expected true or false but found '{value}'"
                        )));
                    }
                })
            }
            ArgumentKind::Integer { min, max } => {
                ArgumentValue::Integer(self.number("integer", *min, *max)?)
            }
            ArgumentKind::Long { min, max } => {
                ArgumentValue::Long(self.number("long", *min, *max)?)
            }
            ArgumentKind::Float { min, max } => {
                ArgumentValue::Float(self.number("float", *min, *max)?)
            }
            ArgumentKind::Double { min, max } => {
                ArgumentValue::Double(self.number("double", *min, *max)?)
            }
            ArgumentKind::Custom(parser) => parser.parse(self)?,
        })
    }
}
