//! Transport-independent command registration, parsing and execution.
//! Sources are trusted server inputs, never data accepted from a client packet.
pub mod arguments;
mod builtins;
mod parse;
mod runtime;
mod source;
mod tree;
pub use parse::{ArgumentValue, Arguments, CommandError, StringReader};
pub use runtime::*;
pub use source::*;
use std::{collections::HashSet, io, sync::Arc};
pub use tree::*;
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
    /// Owned snapshots for entity argument evaluation. Hosts without a world
    /// explicitly report the missing capability rather than returning fake data.
    fn command_entities(&mut self) -> io::Result<Vec<arguments::EntitySnapshot>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Entity queries are unavailable.",
        ))
    }
    /// Optional liveness/policy check before each runtime step and feedback.
    fn command_active(&self, _source: &SourceStack) -> bool {
        true
    }
    /// Only present inside a scheduled executor. Use this instead of invoking
    /// the dispatcher recursively to schedule follow-up work.
    fn execution(&mut self) -> Option<&mut dyn ExecutionControl> {
        None
    }
}

/// Custom types can be used for server-only nodes, or provide a verified
/// client parser descriptor when they implement that parser's wire semantics.
pub trait ArgumentParser: std::fmt::Debug + Send + Sync {
    fn parse(&self, reader: &mut StringReader<'_>) -> Result<ArgumentValue, CommandError>;
    fn examples(&self) -> &[&str] {
        &[]
    }
    fn wire(&self) -> Option<(i32, Vec<u8>)> {
        None
    }
}
#[derive(Debug, Clone)]
pub enum ArgumentKind {
    Word,
    String,
    Greedy,
    Integer { min: i32, max: i32 },
    Long { min: i64, max: i64 },
    Float { min: f32, max: f32 },
    Double { min: f64, max: f64 },
    Bool,
    Custom(Arc<dyn ArgumentParser>),
}
impl ArgumentKind {
    fn examples(&self) -> &[&str] {
        match self {
            Self::Word => &["word", "words_with_underscores"],
            Self::String => &["\"quoted phrase\"", "word", "\"\""],
            Self::Greedy => &["word", "words with spaces", "\"and symbols\""],
            Self::Bool => &["true", "false"],
            Self::Integer { .. } | Self::Long { .. } => &["0", "123", "-123"],
            Self::Float { .. } | Self::Double { .. } => {
                &["0", "1.2", ".5", "-1", "-.5", "-1234.56"]
            }
            Self::Custom(parser) => parser.examples(),
        }
    }
    fn valid(&self) -> bool {
        match self {
            Self::Integer { min, max } => min <= max,
            Self::Long { min, max } => min <= max,
            Self::Float { min, max } => min.is_finite() && max.is_finite() && min <= max,
            Self::Double { min, max } => min.is_finite() && max.is_finite() && min <= max,
            _ => true,
        }
    }
}
#[derive(Debug, Clone)]
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
    pub input: &'a str,
    pub nodes: &'a [ParsedNode],
    /// Present in Minecraft runtime execution, absent in pure Brigadier mode.
    pub stack: Option<&'a SourceStack>,
}
pub type Executor = fn(&Invocation<'_>, &mut dyn CommandHost) -> io::Result<CommandResult>;

/// Convenience registration for a simple argument path with aliases.
/// Use `CommandRegistryBuilder::add_node` for branching graphs.
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

pub struct CommandRegistryBuilder {
    commands: Vec<Command>,
    nodes: Vec<CommandNode>,
    limits: DispatchLimits,
    consumer: Option<ResultConsumer>,
}
impl Default for CommandRegistryBuilder {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            nodes: vec![CommandNode::root()],
            limits: DispatchLimits::default(),
            consumer: None,
        }
    }
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
    pub fn limits(&mut self, limits: DispatchLimits) -> &mut Self {
        self.limits = limits;
        self
    }
    pub fn result_consumer(&mut self, consumer: ResultConsumer) -> &mut Self {
        self.consumer = Some(consumer);
        self
    }

    /// Like Brigadier addChild, keep the first node's type, requirement and
    /// redirect; a subsequent executor replaces the old one. Add descendants
    /// to the returned ID to merge multiple registrations.
    pub fn add_node(&mut self, parent: NodeId, node: CommandNode) -> Result<NodeId, String> {
        if self
            .nodes
            .get(parent.0)
            .is_none_or(|parent| parent.redirect.is_some())
        {
            return Err("invalid parent or redirect cannot have children".into());
        }
        if node.name.is_empty()
            || node.name.encode_utf16().count() > 64
            || node.name.contains(' ')
            || matches!(node.kind, NodeKind::Root)
            || matches!(&node.kind, NodeKind::Argument(kind) if !kind.valid())
            || node
                .redirect
                .is_some_and(|redirect| redirect.target.0 >= self.nodes.len())
        {
            return Err("invalid command node".into());
        }
        if let Some(id) = self.nodes[parent.0]
            .children
            .iter()
            .copied()
            .find(|id| self.nodes[id.0].name == node.name)
        {
            if node.execute.is_some() {
                self.nodes[id.0].execute = node.execute;
            }
            return Ok(id);
        }
        if self.nodes.len() >= MAX_NODES {
            return Err("command registry too large".into());
        }
        let id = NodeId(self.nodes.len());
        self.nodes.push(node);
        self.nodes[parent.0].children.push(id);
        Ok(id)
    }

    pub fn build(mut self) -> Result<CommandRegistry, String> {
        let commands = std::mem::take(&mut self.commands);
        let mut names: HashSet<_> = self.nodes[0]
            .children
            .iter()
            .map(|id| self.nodes[id.0].name.clone())
            .collect();
        for command in &commands {
            if command.arguments.len() > 16
                || command.description.len() > 256
                || !valid_text(&command.description)
            {
                return Err("invalid command definition".into());
            }
            for name in std::iter::once(&command.name).chain(&command.aliases) {
                if !identifier(name) || !names.insert(name.clone()) {
                    return Err(format!("invalid or duplicate command: {name}"));
                }
            }
            let mut args = HashSet::new();
            for (index, argument) in command.arguments.iter().enumerate() {
                if !identifier(&argument.name)
                    || !args.insert(&argument.name)
                    || !argument.kind.valid()
                    || matches!(argument.kind, ArgumentKind::Greedy)
                        && index + 1 != command.arguments.len()
                {
                    return Err("invalid command argument".into());
                }
            }
            let mut literal = CommandNode::literal(&command.name);
            literal.permission = command.permission;
            literal.player_only = command.player_only;
            if command.arguments.is_empty() {
                literal.execute = Some(command.execute);
            }
            let primary = self.add_node(NodeId::ROOT, literal.clone())?;
            for alias in &command.aliases {
                let mut alias_node = literal.clone().redirects(primary, None);
                alias_node.name.clone_from(alias);
                self.add_node(NodeId::ROOT, alias_node)?;
            }
            let mut parent = primary;
            for (index, arg) in command.arguments.iter().enumerate() {
                let mut node = CommandNode::argument(&arg.name, arg.kind.clone());
                if index + 1 == command.arguments.len() {
                    node.execute = Some(command.execute);
                }
                parent = self.add_node(parent, node)?;
            }
        }
        Ok(CommandRegistry {
            commands,
            nodes: self.nodes,
            limits: self.limits,
            consumer: self.consumer,
        })
    }
}

/// Immutable graph shared by dispatch, help and wire encoding.
pub struct CommandRegistry {
    commands: Vec<Command>,
    nodes: Vec<CommandNode>,
    limits: DispatchLimits,
    consumer: Option<ResultConsumer>,
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
        let known: HashSet<_> = self
            .commands
            .iter()
            .flat_map(|command| {
                std::iter::once(command.name.as_str())
                    .chain(command.aliases.iter().map(String::as_str))
            })
            .collect();
        for (id, usage) in self.smart_usage(NodeId::ROOT, source) {
            if known.contains(self.nodes[id.0].name.as_str()) {
                continue;
            }
            if text.len() + usage.len() > 8192 {
                text.push_str("; …");
                break;
            }
            if !text.is_empty() {
                text.push_str("; ");
            }
            text.push('/');
            text.push_str(&usage);
        }
        text
    }
    pub fn execute(
        &self,
        input: &str,
        source: &CommandSource,
        host: &mut dyn CommandHost,
    ) -> io::Result<CommandResult> {
        match self.server_parse(input, source) {
            Ok(parsed) => parsed.execute(host),
            Err(result) => Ok(result),
        }
    }

    fn server_parse<'a>(
        &'a self,
        input: &'a str,
        source: &CommandSource,
    ) -> Result<ParseResults<'a>, CommandResult> {
        if input.len() > MAX_COMMAND_BYTES {
            return Err(CommandResult::failure("Invalid command text."));
        }
        let input = input.trim();
        let input = input.strip_prefix('/').unwrap_or(input);
        // Explicit, friendly source errors for the server adapter. The
        // Brigadier dispatcher itself skips inaccessible nodes.
        let name = input.split(' ').next().unwrap_or("");
        if let Some(node) = self.find_node(&[name]).and_then(|id| self.node(id)) {
            if node.permission > source.permission {
                return Err(CommandResult::failure("Permission denied."));
            }
            if node.player_only && !matches!(source.kind, SourceKind::Player(_)) {
                return Err(CommandResult::failure("This command requires a player."));
            }
        }
        let parsed = self.dispatch(input, source);
        if parsed.contexts()[0].nodes.is_empty() && parsed.errors().is_empty() {
            return Err(CommandResult::failure("Unknown command. Use /help."));
        }
        Ok(parsed)
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
