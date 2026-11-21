use super::{Argument, ArgumentKind};
use std::{collections::BTreeMap, fmt, ops::Range};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentValue {
    Text(String),
    Integer(i32),
    Bool(bool),
}
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Arguments(BTreeMap<String, ArgumentValue>);
impl Arguments {
    pub fn get(&self, name: &str) -> Option<&ArgumentValue> {
        self.0.get(name)
    }
    pub fn text(&self, name: &str) -> Option<&str> {
        match self.get(name) {
            Some(ArgumentValue::Text(value)) => Some(value),
            _ => None,
        }
    }
    pub fn integer(&self, name: &str) -> Option<i32> {
        match self.get(name) {
            Some(ArgumentValue::Integer(value)) => Some(*value),
            _ => None,
        }
    }
    pub fn boolean(&self, name: &str) -> Option<bool> {
        match self.get(name) {
            Some(ArgumentValue::Bool(value)) => Some(*value),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub message: String,
    /// UTF-8 byte offsets into the original command, not character indices.
    pub span: Range<usize>,
    pub usage: Option<String>,
}
impl CommandError {
    pub(super) fn new(message: impl Into<String>, span: Range<usize>) -> Self {
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

pub(super) struct Cursor<'a> {
    input: &'a str,
    pos: usize,
}
impl<'a> Cursor<'a> {
    pub fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }
    pub fn space(&mut self) {
        while self.input.as_bytes().get(self.pos) == Some(&b' ') {
            self.pos += 1;
        }
    }
    pub fn slash(&mut self) {
        if self.input.as_bytes().get(self.pos) == Some(&b'/') {
            self.pos += 1;
        }
    }
    pub fn word(&mut self) -> Result<(String, Range<usize>), CommandError> {
        self.space();
        let start = self.pos;
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b' ' {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(CommandError::new("Expected an argument.", start..start));
        }
        let text = &self.input[start..self.pos];
        if !text
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'+'))
        {
            return Err(CommandError::new(
                "Expected an unquoted word (or a quoted string where supported).",
                start..self.pos,
            ));
        }
        Ok((text.into(), start..self.pos))
    }
    fn string(&mut self) -> Result<(String, Range<usize>), CommandError> {
        self.space();
        let start = self.pos;
        if self.input.as_bytes().get(self.pos) != Some(&b'"') {
            return self.word();
        }
        self.pos += 1;
        let mut value = String::new();
        while let Some(ch) = self.input[self.pos..].chars().next() {
            self.pos += ch.len_utf8();
            match ch {
                '"' => {
                    if self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b' ' {
                        return Err(CommandError::new(
                            "Expected space after quoted string.",
                            start..self.pos,
                        ));
                    }
                    return Ok((value, start..self.pos));
                }
                '\\' => {
                    let next = self.input[self.pos..].chars().next();
                    match next {
                        Some(next @ ('\\' | '"')) => {
                            self.pos += 1;
                            value.push(next);
                        }
                        _ => {
                            return Err(CommandError::new(
                                "Invalid quoted escape.",
                                self.pos - 1..self.pos,
                            ));
                        }
                    }
                }
                _ => value.push(ch),
            }
        }
        Err(CommandError::new(
            "Unterminated quoted string.",
            start..self.pos,
        ))
    }
    pub fn arguments(&mut self, definitions: &[Argument]) -> Result<Arguments, CommandError> {
        let mut result = Arguments::default();
        for argument in definitions {
            self.space();
            let start = self.pos;
            let (text, span) = match argument.kind {
                ArgumentKind::String => self.string()?,
                ArgumentKind::Greedy => {
                    let end = self.input.trim_end_matches(' ').len();
                    if end <= start {
                        return Err(CommandError::new("Expected an argument.", start..start));
                    }
                    self.pos = end;
                    (self.input[start..end].into(), start..end)
                }
                _ => self.word()?,
            };
            let value = match argument.kind {
                ArgumentKind::Integer { min, max } => {
                    let value = text
                        .parse::<i32>()
                        .ok()
                        .filter(|value| (min..=max).contains(value))
                        .ok_or_else(|| {
                            CommandError::new(format!("Expected integer in {min}..={max}."), span)
                        })?;
                    ArgumentValue::Integer(value)
                }
                ArgumentKind::Bool => ArgumentValue::Bool(match text.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(CommandError::new("Expected true or false.", span)),
                }),
                _ => ArgumentValue::Text(text),
            };
            result.0.insert(argument.name.clone(), value);
        }
        self.space();
        if self.pos != self.input.len() {
            return Err(CommandError::new(
                "Unexpected trailing argument.",
                self.pos..self.input.len(),
            ));
        }
        Ok(result)
    }
}
