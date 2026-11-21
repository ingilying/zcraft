//! Transport-independent command registration, parsing and execution.
//! Sources are trusted server inputs, never data accepted from a client packet.
mod builtins;
mod parse;
pub use parse::{ArgumentValue, Arguments, CommandError};
use std::{collections::HashSet, io};
use uuid::Uuid;

pub const MAX_COMMAND_BYTES: usize = 4096;
const MAX_NODES: usize = 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum SourceKind {
    Player(Uuid),
    Console,
    CommandBlock,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub world: String,
    pub position: [f64; 3],
}

/// Embedders assign permission and origin; the network always uses level zero.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandSource {
    pub kind: SourceKind,
    pub permission: u8,
    pub origin: Option<Origin>,
}
impl CommandSource {
    pub fn player(uuid: Uuid) -> Self {
        Self {
            kind: SourceKind::Player(uuid),
            permission: 0,
            origin: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub success: bool,
    /// Numeric result for future conditional blocks / result consumers.
    pub value: i32,
    pub feedback: Vec<String>,
}
impl CommandResult {
    pub fn success(value: i32) -> Self {
        Self {
            success: true,
            value,
            feedback: Vec::new(),
        }
    }
    pub fn message(text: impl Into<String>) -> Self {
        Self {
            feedback: vec![text.into()],
            ..Self::success(1)
        }
    }
    pub fn failure(text: impl Into<String>) -> Self {
        Self {
            success: false,
            value: 0,
            feedback: vec![text.into()],
        }
    }
}

/// Server capabilities used by builtins. Other callbacks may be pure commands.
/// No Session, socket, packet or protocol state is exposed.
pub trait CommandHost {
    fn list_players(&mut self) -> io::Result<CommandResult>;
    fn action(&mut self, player: Uuid, text: &str) -> io::Result<CommandResult>;
    fn whisper(&mut self, player: Uuid, target: &str, text: &str) -> io::Result<CommandResult>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentKind {
    Word,
    /// Brigadier quotable phrase: a word, or a double-quoted string with \\ and \" escapes.
    String,
    Greedy,
    Integer {
        min: i32,
        max: i32,
    },
    Bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argument {
    pub name: String,
    pub kind: ArgumentKind,
}
impl Argument {
    pub fn new(name: impl Into<String>, kind: ArgumentKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

pub struct Invocation<'a> {
    pub source: &'a CommandSource,
    pub arguments: &'a Arguments,
    pub registry: &'a CommandRegistry,
}
pub type Executor = fn(&Invocation<'_>, &mut dyn CommandHost) -> io::Result<CommandResult>;

/// A command has one ordered, required argument path. Aliases share this path
/// and executor. Optional arguments / branching subcommands are not implemented.
#[derive(Clone)]
pub struct Command {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub permission: u8,
    pub player_only: bool,
    pub arguments: Vec<Argument>,
    pub execute: Executor,
}
impl Command {
    pub fn new(name: impl Into<String>, description: impl Into<String>, execute: Executor) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            description: description.into(),
            permission: 0,
            player_only: false,
            arguments: Vec::new(),
            execute,
        }
    }
    pub fn argument(mut self, name: impl Into<String>, kind: ArgumentKind) -> Self {
        self.arguments.push(Argument::new(name, kind));
        self
    }
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }
    pub fn permission(mut self, level: u8) -> Self {
        self.permission = level;
        self
    }
    pub fn player_only(mut self) -> Self {
        self.player_only = true;
        self
    }
    pub fn usage(&self) -> String {
        let mut usage = format!("/{}", self.name);
        for argument in &self.arguments {
            usage.push_str(&format!(" <{}>", argument.name));
        }
        usage
    }
}

#[derive(Default)]
pub struct CommandRegistryBuilder {
    commands: Vec<Command>,
}
impl CommandRegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_builtins() -> Self {
        builtins::builder()
    }
    pub fn register(&mut self, command: Command) -> &mut Self {
        self.commands.push(command);
        self
    }
    /// Freeze definitions and reject ambiguity before serving clients.
    pub fn build(self) -> Result<CommandRegistry, String> {
        let mut names = HashSet::new();
        let mut nodes = 1;
        for command in &self.commands {
            nodes += 1 + command.aliases.len() + command.arguments.len();
            if nodes > MAX_NODES || command.arguments.len() > 16 {
                return Err("command registry too large".into());
            }
            if command.description.len() > 256 || !valid_text(&command.description) {
                return Err("invalid command description".into());
            }
            for name in std::iter::once(&command.name).chain(&command.aliases) {
                if !identifier(name) || !names.insert(name.clone()) {
                    return Err(format!("invalid or duplicate command: {name}"));
                }
            }
            let mut args = HashSet::new();
            for (index, argument) in command.arguments.iter().enumerate() {
                if !identifier(&argument.name) || !args.insert(&argument.name) {
                    return Err("invalid or duplicate argument".into());
                }
                match argument.kind {
                    ArgumentKind::Greedy if index + 1 != command.arguments.len() => {
                        return Err("greedy argument must be last".into());
                    }
                    ArgumentKind::Integer { min, max } if min > max => {
                        return Err("invalid integer bounds".into());
                    }
                    _ => {}
                }
            }
        }
        Ok(CommandRegistry {
            commands: self.commands,
        })
    }
}

/// Immutable after validation. Shared by dispatch, help and the protocol tree.
pub struct CommandRegistry {
    commands: Vec<Command>,
}
impl Default for CommandRegistry {
    fn default() -> Self {
        CommandRegistryBuilder::with_builtins()
            .build()
            .expect("valid builtin commands")
    }
}
impl CommandRegistry {
    pub fn available<'a>(&'a self, source: &'a CommandSource) -> impl Iterator<Item = &'a Command> {
        self.commands
            .iter()
            .filter(|command| allowed(command, source))
    }
    pub fn help(&self, source: &CommandSource) -> String {
        // Output is bounded independently of registry size and the NBT limit.
        let mut text = String::new();
        for command in self.available(source) {
            let aliases = if command.aliases.is_empty() {
                String::new()
            } else {
                format!(" (aliases: {})", command.aliases.join(", "))
            };
            let line = format!("{} — {}{}", command.usage(), command.description, aliases);
            if text.len() + line.len() > 8192 {
                text.push_str("; …");
                break;
            }
            if !text.is_empty() {
                text.push_str("; ");
            }
            text.push_str(&line);
        }
        text
    }
    pub fn execute(
        &self,
        input: &str,
        source: &CommandSource,
        host: &mut dyn CommandHost,
    ) -> io::Result<CommandResult> {
        let (command, arguments) = match self.parse(input, source) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(CommandResult::failure(error.to_string())),
        };
        (command.execute)(
            &Invocation {
                source,
                arguments: &arguments,
                registry: self,
            },
            host,
        )
    }
    pub fn parse<'a>(
        &'a self,
        input: &str,
        source: &CommandSource,
    ) -> Result<(&'a Command, Arguments), CommandError> {
        if input.len() > MAX_COMMAND_BYTES || !valid_text(input) {
            return Err(CommandError::new("Invalid command text.", 0..input.len()));
        }
        let mut cursor = parse::Cursor::new(input);
        cursor.space();
        cursor.slash();
        let (name, span) = cursor.word()?;
        let command = self
            .commands
            .iter()
            .find(|command| {
                command.name == name || command.aliases.iter().any(|alias| alias == &name)
            })
            .ok_or_else(|| CommandError::new("Unknown command. Use /help.", span.clone()))?;
        if command.permission > source.permission {
            return Err(CommandError::new("Permission denied.", span));
        }
        if command.player_only && !matches!(source.kind, SourceKind::Player(_)) {
            return Err(CommandError::new("This command requires a player.", span));
        }
        let args = cursor.arguments(&command.arguments).map_err(|mut error| {
            error.usage = Some(command.usage());
            error
        })?;
        Ok((command, args))
    }
}
fn allowed(command: &Command, source: &CommandSource) -> bool {
    command.permission <= source.permission
        && (!command.player_only || matches!(source.kind, SourceKind::Player(_)))
}
fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}
/// Shared text safety; player packet length limits remain in the protocol codec.
pub fn valid_text(text: &str) -> bool {
    !text.chars().any(|c| c.is_control() || matches!(c, '§' | '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}

#[cfg(test)]
mod tests;
