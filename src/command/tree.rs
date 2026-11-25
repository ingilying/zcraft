//! Node graph and dispatcher, based on Brigadier 1.3.10 behavior.
use super::*;
use std::{collections::BTreeSet, future::Future, ops::Range, pin::Pin};

mod completion;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(super) usize);
impl NodeId {
    pub const ROOT: Self = Self(0);
}

pub type RedirectModifier = fn(&Invocation<'_>) -> Result<Vec<CommandSource>, CommandError>;
pub type StackModifier =
    fn(&Invocation<'_>, &SourceStack) -> Result<Vec<SourceStack>, CommandError>;
pub type ResultConsumer = fn(&Invocation<'_>, bool, i32);
pub type SuggestionProvider = fn(&SuggestionContext<'_>) -> Vec<Suggestion>;
pub type SuggestionFuture<'a> =
    Pin<Box<dyn Future<Output = io::Result<Vec<Suggestion>>> + Send + 'a>>;
pub type AsyncSuggestionProvider = for<'a> fn(SuggestionContext<'a>) -> SuggestionFuture<'a>;

// Bound retained output independent of any custom provider.
pub const MAX_SUGGESTIONS: usize = 128;
pub const MAX_SUGGESTION_BYTES: usize = 1024;

#[derive(Clone, Copy)]
pub struct Redirect {
    pub target: NodeId,
    pub modifier: Option<RedirectModifier>,
    pub fork: bool,
    stack_modifier: Option<StackModifier>,
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Root,
    Literal,
    Argument(ArgumentKind),
}

#[derive(Clone)]
pub struct CommandNode {
    pub(super) name: String,
    pub(super) kind: NodeKind,
    pub(super) requirement: fn(&CommandSource) -> bool,
    pub(super) permission: u8,
    pub(super) player_only: bool,
    pub(super) execute: Option<Executor>,
    pub(super) redirect: Option<Redirect>,
    pub(super) suggestions: Option<SuggestionProvider>,
    async_suggestions: Option<AsyncSuggestionProvider>,
    pub(super) children: Vec<NodeId>,
}
impl CommandNode {
    pub(super) fn root() -> Self {
        Self::new("", NodeKind::Root)
    }
    pub fn literal(name: impl Into<String>) -> Self {
        Self::new(name, NodeKind::Literal)
    }
    pub fn argument(name: impl Into<String>, kind: ArgumentKind) -> Self {
        Self::new(name, NodeKind::Argument(kind))
    }
    fn new(name: impl Into<String>, kind: NodeKind) -> Self {
        Self {
            name: name.into(),
            kind,
            requirement: |_| true,
            permission: 0,
            player_only: false,
            execute: None,
            redirect: None,
            suggestions: None,
            async_suggestions: None,
            children: Vec::new(),
        }
    }
    pub fn requires(mut self, predicate: fn(&CommandSource) -> bool) -> Self {
        self.requirement = predicate;
        self
    }
    pub fn executes(mut self, execute: Executor) -> Self {
        self.execute = Some(execute);
        self
    }
    pub fn redirects(mut self, target: NodeId, modifier: Option<RedirectModifier>) -> Self {
        self.redirect = Some(Redirect {
            target,
            modifier,
            fork: false,
            stack_modifier: None,
        });
        self
    }
    pub fn forks(mut self, target: NodeId, modifier: RedirectModifier) -> Self {
        self.redirect = Some(Redirect {
            target,
            modifier: Some(modifier),
            fork: true,
            stack_modifier: None,
        });
        self
    }
    /// Redirect with a Minecraft source transform. Requires `CommandRegistry::run`.
    pub fn redirects_stack(mut self, target: NodeId, modifier: StackModifier, fork: bool) -> Self {
        self.redirect = Some(Redirect {
            target,
            modifier: None,
            fork,
            stack_modifier: Some(modifier),
        });
        self
    }
    pub fn suggests(mut self, provider: SuggestionProvider) -> Self {
        self.suggestions = Some(provider);
        self
    }
    /// Install a cooperative asynchronous provider. If a synchronous provider
    /// is also installed, it is used by the synchronous completion API only.
    pub fn suggests_async(mut self, provider: AsyncSuggestionProvider) -> Self {
        self.async_suggestions = Some(provider);
        self
    }
    pub fn has_suggestion_provider(&self) -> bool {
        self.suggestions.is_some() || self.async_suggestions.is_some()
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }
    pub fn executable(&self) -> bool {
        self.execute.is_some()
    }
    pub fn redirect(&self) -> Option<Redirect> {
        self.redirect
    }
    pub fn can_use(&self, source: &CommandSource) -> bool {
        self.permission <= source.permission
            && (!self.player_only || matches!(source.kind, SourceKind::Player(_)))
            && (self.requirement)(source)
    }
    fn usage(&self) -> String {
        if matches!(self.kind, NodeKind::Argument(_)) {
            format!("<{}>", self.name)
        } else {
            self.name.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ambiguity {
    pub parent: NodeId,
    pub first: NodeId,
    pub second: NodeId,
    pub inputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNode {
    pub id: NodeId,
    pub span: Range<usize>,
}
#[derive(Debug, Clone)]
pub struct ParsedContext {
    pub root: NodeId,
    pub start: usize,
    pub nodes: Vec<ParsedNode>,
    pub arguments: Arguments,
}
impl ParsedContext {
    fn new(root: NodeId, start: usize) -> Self {
        Self {
            root,
            start,
            nodes: Vec::new(),
            arguments: Arguments::default(),
        }
    }
}

/// A partial parse is useful even when it cannot be executed (for completion).
/// Borrows its registry, so node IDs cannot accidentally be executed elsewhere.
pub struct ParseResults<'a> {
    registry: &'a CommandRegistry,
    input: &'a str,
    source: CommandSource,
    state: Parsed,
}
#[derive(Clone)]
struct Parsed {
    contexts: Vec<ParsedContext>,
    cursor: usize,
    errors: Vec<(NodeId, CommandError)>,
    budget_exhausted: bool,
}
impl ParseResults<'_> {
    pub fn cursor(&self) -> usize {
        self.state.cursor
    }
    pub fn contexts(&self) -> &[ParsedContext] {
        &self.state.contexts
    }
    pub fn errors(&self) -> &[(NodeId, CommandError)] {
        &self.state.errors
    }
    pub fn input(&self) -> &str {
        self.input
    }
    pub fn error(&self) -> Option<CommandError> {
        if self.state.budget_exhausted {
            return Some(CommandError::new(
                "Command parsing budget exceeded",
                self.cursor()..self.cursor(),
            ));
        }
        if self.cursor() < self.input.encode_utf16().count() {
            if let [(_, error)] = self.errors() {
                return Some(error.clone());
            }
            let msg = if self.contexts()[0].nodes.is_empty() {
                "Unknown command"
            } else {
                "Incorrect argument for command"
            };
            return Some(CommandError::new(msg, self.cursor()..self.cursor()));
        }
        if self.executable().is_none() {
            return Some(CommandError::new(
                "Unknown command",
                self.cursor()..self.cursor(),
            ));
        }
        None
    }
    fn executable(&self) -> Option<Executor> {
        self.contexts()
            .last()?
            .nodes
            .last()
            .and_then(|node| self.registry.node(node.id)?.execute)
    }
    pub fn execute(&self, host: &mut dyn CommandHost) -> io::Result<CommandResult> {
        if host.execution().is_some() {
            return Err(io::Error::other(
                "enqueue commands instead of dispatching recursively",
            ));
        }
        if let Some(error) = self.error() {
            if !self.state.budget_exhausted && self.cursor() == self.input.encode_utf16().count() {
                self.complete(
                    &self.invocation(&self.source, &self.contexts()[0]),
                    false,
                    0,
                );
            }
            return Ok(CommandResult::failure(error.to_string()));
        }
        let mut sources = vec![self.source.clone()];
        let mut forked = false;
        let mut output = CommandResult::success(0);
        let contexts = self.contexts();
        for context in &contexts[..contexts.len() - 1] {
            let redirect = self.registry.nodes[context.nodes.last().expect("redirect node").id.0]
                .redirect
                .expect("parsed redirect");
            if redirect.stack_modifier.is_some() {
                return Ok(CommandResult::failure(
                    "This redirect requires Minecraft command execution.",
                ));
            }
            forked |= redirect.fork;
            let mut next = Vec::new();
            for source in sources {
                let invocation = self.invocation(&source, context);
                if let Some(modify) = redirect.modifier {
                    match modify(&invocation) {
                        Ok(mut modified) => {
                            if next.len().saturating_add(modified.len())
                                > self.registry.limits.sources
                            {
                                return Ok(CommandResult::failure(
                                    "Command source budget exceeded",
                                ));
                            }
                            next.append(&mut modified);
                        }
                        Err(error) => {
                            self.complete(&invocation, false, 0);
                            if !forked {
                                return Ok(CommandResult::failure(error.to_string()));
                            }
                        }
                    }
                } else {
                    next.push(source);
                }
            }
            sources = next;
        }
        let context = contexts.last().expect("root context");
        let execute = self.executable().expect("validated executor");
        for source in sources {
            let invocation = self.invocation(&source, context);
            let result = execute(&invocation, host)?;
            self.complete(
                &invocation,
                result.success,
                if result.success { result.value } else { 0 },
            );
            if !result.success && !forked {
                return Ok(result);
            }
            if result.success {
                output.value = output
                    .value
                    .wrapping_add(if forked { 1 } else { result.value });
            }
            output.feedback.extend(result.feedback);
        }
        Ok(output)
    }
    pub(super) fn execute_runtime(
        &self,
        stack: &SourceStack,
        host: &mut dyn CommandHost,
        runner: &mut runtime::Runner,
        frame: usize,
    ) -> Result<CommandResult, ExecutionFailure> {
        if let Some(error) = self.error() {
            if !self.state.budget_exhausted && self.cursor() == self.input.encode_utf16().count() {
                self.complete(
                    &self.runtime_invocation(stack, &self.contexts()[0]),
                    false,
                    0,
                );
            }
            return Ok(CommandResult::failure(error.to_string()));
        }
        let mut sources = vec![stack.clone()];
        let mut forked = false;
        let contexts = self.contexts();
        for context in &contexts[..contexts.len() - 1] {
            let redirect = self.registry.nodes[context.nodes.last().expect("redirect node").id.0]
                .redirect
                .expect("parsed redirect");
            forked |= redirect.fork;
            let mut next = Vec::new();
            for source in sources {
                if let Err(error) = runner.step(host, &source) {
                    runner.complete_source(&source, false, 0);
                    return Err(error);
                }
                let call = self.runtime_invocation(&source, context);
                let modified = if !self.context_allowed(context, &source) {
                    Err(CommandError::new(
                        "Permission denied.",
                        context.start..context.start,
                    ))
                } else if let Some(modify) = redirect.stack_modifier {
                    modify(&call, &source).map(|mut stacks| {
                        for stack in &mut stacks {
                            stack.restrict_to(&source);
                        }
                        stacks
                    })
                } else if let Some(modify) = redirect.modifier {
                    modify(&call).map(|sources| {
                        sources
                            .into_iter()
                            .map(|s| source.legacy_transform(s))
                            .collect()
                    })
                } else {
                    Ok(vec![source.clone()])
                };
                match modified {
                    Ok(stacks) => {
                        runner.expand(
                            stacks
                                .len()
                                .min(self.registry.limits.sources.saturating_add(1)),
                        )?;
                        if next.len().saturating_add(stacks.len()) > self.registry.limits.sources {
                            return Err(ExecutionFailure::Sources);
                        }
                        next.extend(stacks);
                    }
                    Err(error) => {
                        self.complete(&call, false, 0);
                        runner.complete_source(&source, false, 0);
                        if !forked {
                            return Ok(CommandResult::failure(error.to_string()));
                        }
                    }
                }
            }
            sources = next;
        }
        let context = contexts.last().expect("root context");
        let execute = self.executable().expect("validated executor");
        let mut output = CommandResult::success(0);
        let mut any_success = false;
        for source in sources {
            let call = self.runtime_invocation(&source, context);
            let result = if self.context_allowed(context, &source) {
                match runner.invoke(frame, &source, host, execute, &call) {
                    Ok(result) => result,
                    Err(error) => {
                        self.complete(&call, false, 0);
                        runner.complete_source(&source, false, 0);
                        return Err(error);
                    }
                }
            } else {
                if let Err(error) = runner.step(host, &source) {
                    runner.complete_source(&source, false, 0);
                    return Err(error);
                }
                CommandResult::failure("Permission denied.")
            };
            self.complete(
                &call,
                result.success,
                if result.success { result.value } else { 0 },
            );
            runner.complete_execution(&source, &result);
            runner.feedback(&source, result.feedback)?;
            if !result.success && !forked {
                return Ok(CommandResult {
                    success: false,
                    value: 0,
                    feedback: Vec::new(),
                });
            }
            if result.success {
                any_success = true;
                output.value = output
                    .value
                    .wrapping_add(if forked { 1 } else { result.value });
            }
            if runner.returned() {
                break;
            }
        }
        output.success = any_success;
        Ok(output)
    }
    fn context_allowed(&self, context: &ParsedContext, source: &SourceStack) -> bool {
        context
            .nodes
            .iter()
            .all(|node| self.registry.nodes[node.id.0].can_use(source.source()))
    }
    fn runtime_invocation<'a>(
        &'a self,
        stack: &'a SourceStack,
        context: &'a ParsedContext,
    ) -> Invocation<'a> {
        Invocation {
            stack: Some(stack),
            ..self.invocation(stack.source(), context)
        }
    }
    fn invocation<'a>(
        &'a self,
        source: &'a CommandSource,
        context: &'a ParsedContext,
    ) -> Invocation<'a> {
        Invocation {
            source,
            arguments: &context.arguments,
            registry: self.registry,
            input: self.input,
            nodes: &context.nodes,
            stack: None,
        }
    }
    fn complete(&self, invocation: &Invocation<'_>, success: bool, value: i32) {
        if let Some(consumer) = self.registry.consumer {
            consumer(invocation, success, value);
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DispatchLimits {
    pub visits: usize,
    pub depth: usize,
    pub sources: usize,
}
impl Default for DispatchLimits {
    fn default() -> Self {
        Self {
            visits: 8192,
            depth: 128,
            sources: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Suggestion {
    pub span: Range<usize>,
    pub text: String,
    pub tooltip: Option<String>,
}
#[derive(Clone, Copy)]
pub struct SuggestionContext<'a> {
    pub source: &'a CommandSource,
    pub arguments: &'a Arguments,
    pub input: &'a [u16],
    pub start: usize,
}
impl SuggestionContext<'_> {
    pub fn remaining(&self) -> String {
        String::from_utf16_lossy(&self.input[self.start..])
    }
    pub fn suggest(&self, text: impl Into<String>) -> Suggestion {
        Suggestion {
            span: self.start..self.input.len(),
            text: text.into(),
            tooltip: None,
        }
    }
}

impl CommandRegistry {
    pub fn node(&self, id: NodeId) -> Option<&CommandNode> {
        self.nodes.get(id.0)
    }
    pub fn find_node(&self, path: &[&str]) -> Option<NodeId> {
        path.iter().try_fold(NodeId::ROOT, |id, name| {
            self.node(id)?
                .children
                .iter()
                .copied()
                .find(|child| self.nodes[child.0].name == *name)
        })
    }
    pub fn path(&self, id: NodeId) -> Option<Vec<&str>> {
        fn visit(
            registry: &CommandRegistry,
            node: NodeId,
            target: NodeId,
            path: &mut Vec<NodeId>,
        ) -> bool {
            if node == target {
                return true;
            }
            for &child in &registry.nodes[node.0].children {
                path.push(child);
                if visit(registry, child, target, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
        let mut path = Vec::new();
        visit(self, NodeId::ROOT, id, &mut path)
            .then(|| path.into_iter().map(|id| self.nodes[id.0].name()).collect())
    }
    /// Report overlapping sibling parsers from their advertised examples.
    /// As in Brigadier this is a diagnostic, not an exhaustive proof.
    pub fn find_ambiguities(&self) -> Vec<Ambiguity> {
        fn visit(registry: &CommandRegistry, parent: NodeId, out: &mut Vec<Ambiguity>) {
            let children = &registry.nodes[parent.0].children;
            for &first in children {
                let node = &registry.nodes[first.0];
                let examples = match &node.kind {
                    NodeKind::Root => &[][..],
                    NodeKind::Literal => &[node.name.as_str()][..],
                    NodeKind::Argument(kind) => kind.examples(),
                };
                for &second in children.iter().filter(|&&second| second != first) {
                    let sibling = &registry.nodes[second.0];
                    let inputs: Vec<_> = examples
                        .iter()
                        .copied()
                        .filter(|input| match &sibling.kind {
                            NodeKind::Root => false,
                            NodeKind::Literal => input.split(' ').next() == Some(sibling.name()),
                            NodeKind::Argument(kind) => {
                                let units: Vec<_> = input.encode_utf16().collect();
                                let mut reader = StringReader::new(&units);
                                reader.argument(kind).is_ok()
                                    && reader.peek().is_none_or(|unit| unit == 32)
                            }
                        })
                        .map(str::to_owned)
                        .collect();
                    if !inputs.is_empty() {
                        out.push(Ambiguity {
                            parent,
                            first,
                            second,
                            inputs,
                        });
                    }
                }
                visit(registry, first, out);
            }
        }
        let mut out = Vec::new();
        visit(self, NodeId::ROOT, &mut out);
        out
    }

    /// Exact input: no slash stripping, trimming, or chat filtering.
    pub fn dispatch<'a>(&'a self, input: &'a str, source: &CommandSource) -> ParseResults<'a> {
        self.dispatch_at(input, 0, source)
    }
    pub fn dispatch_at<'a>(
        &'a self,
        input: &'a str,
        cursor: usize,
        source: &CommandSource,
    ) -> ParseResults<'a> {
        let units: Vec<_> = input.encode_utf16().collect();
        let mut reader = StringReader::new(&units);
        reader.set_cursor(cursor);
        let mut visits = self.limits.visits;
        let state = self.parse_nodes(
            NodeId::ROOT,
            reader,
            ParsedContext::new(NodeId::ROOT, cursor.min(units.len())),
            source,
            &mut visits,
            0,
        );
        ParseResults {
            registry: self,
            input,
            source: source.clone(),
            state,
        }
    }
    fn parse_nodes(
        &self,
        parent: NodeId,
        reader: StringReader<'_>,
        context: ParsedContext,
        source: &CommandSource,
        visits: &mut usize,
        depth: usize,
    ) -> Parsed {
        let cursor = reader.cursor();
        if depth >= self.limits.depth || *visits == 0 {
            return Parsed {
                contexts: vec![context],
                cursor,
                errors: vec![],
                budget_exhausted: true,
            };
        }
        let mut errors = Vec::new();
        let mut best: Option<Parsed> = None;
        let children = &self.nodes[parent.0].children;
        let word = String::from_utf16_lossy(
            &reader.remaining()[..reader
                .remaining()
                .iter()
                .position(|&u| u == 32)
                .unwrap_or(reader.remaining().len())],
        );
        let literal = children.iter().copied().find(|id| {
            matches!(self.nodes[id.0].kind, NodeKind::Literal) && self.nodes[id.0].name == word
        });
        for child in children.iter().copied().filter(|&id| {
            literal.map_or_else(
                || matches!(self.nodes[id.0].kind, NodeKind::Argument(_)),
                |lit| id == lit,
            )
        }) {
            if !self.nodes[child.0].can_use(source) {
                continue;
            }
            if *visits == 0 {
                return Parsed {
                    contexts: vec![context],
                    cursor,
                    errors,
                    budget_exhausted: true,
                };
            }
            *visits -= 1;
            let node = &self.nodes[child.0];
            let mut next = reader.clone();
            let mut parsed = context.clone();
            if let NodeKind::Argument(kind) = &node.kind {
                match next.argument(kind) {
                    Ok(value) => {
                        parsed
                            .arguments
                            .0
                            .insert(node.name.clone(), (value, cursor..next.cursor()));
                    }
                    Err(error) => {
                        errors.push((child, error));
                        continue;
                    }
                }
            } else {
                next.set_cursor(cursor + node.name.encode_utf16().count());
            }
            if next.peek().is_some_and(|unit| unit != 32) {
                errors.push((
                    child,
                    next.error("Expected whitespace to end one argument, but found trailing data"),
                ));
                continue;
            }
            parsed.nodes.push(ParsedNode {
                id: child,
                span: cursor..next.cursor(),
            });
            let potential = if let Some(redirect) = node.redirect
                && next.peek().is_some()
            {
                next.read();
                let child_context = ParsedContext::new(redirect.target, next.cursor());
                let mut result = self.parse_nodes(
                    redirect.target,
                    next,
                    child_context,
                    source,
                    visits,
                    depth + 1,
                );
                result.contexts.insert(0, parsed);
                return result;
            } else if next.remaining().len() >= 2 {
                next.read();
                self.parse_nodes(child, next, parsed, source, visits, depth + 1)
            } else {
                Parsed {
                    contexts: vec![parsed],
                    cursor: next.cursor(),
                    errors: vec![],
                    budget_exhausted: false,
                }
            };
            if potential.budget_exhausted {
                return potential;
            }
            let rank = |p: &Parsed| {
                (
                    p.cursor == cursor + reader.remaining().len(),
                    p.errors.is_empty(),
                )
            };
            if best.as_ref().is_none_or(|old| rank(&potential) > rank(old)) {
                best = Some(potential);
            }
        }
        best.unwrap_or(Parsed {
            contexts: vec![context],
            cursor,
            errors,
            budget_exhausted: false,
        })
    }
    pub fn all_usage(
        &self,
        start: NodeId,
        source: &CommandSource,
        restricted: bool,
    ) -> Vec<String> {
        fn walk(
            reg: &CommandRegistry,
            id: NodeId,
            prefix: &str,
            s: &CommandSource,
            restricted: bool,
            out: &mut Vec<String>,
        ) {
            let node = &reg.nodes[id.0];
            if restricted && !node.can_use(s) {
                return;
            }
            if node.executable() {
                out.push(prefix.into());
            }
            if let Some(redirect) = node.redirect {
                let target = if redirect.target == NodeId::ROOT {
                    "...".into()
                } else {
                    format!("-> {}", reg.nodes[redirect.target.0].usage())
                };
                out.push(join(prefix, &target));
            } else {
                for &child in &node.children {
                    walk(
                        reg,
                        child,
                        &join(prefix, &reg.nodes[child.0].usage()),
                        s,
                        restricted,
                        out,
                    );
                }
            }
        }
        let mut result = Vec::new();
        if self.node(start).is_some() {
            walk(self, start, "", source, restricted, &mut result);
        }
        result
    }
    pub fn smart_usage(&self, start: NodeId, source: &CommandSource) -> Vec<(NodeId, String)> {
        fn usage(
            reg: &CommandRegistry,
            id: NodeId,
            source: &CommandSource,
            optional: bool,
            deep: bool,
        ) -> Option<String> {
            let node = &reg.nodes[id.0];
            if !node.can_use(source) {
                return None;
            }
            let label = node.usage();
            let here = if optional {
                format!("[{label}]")
            } else {
                label
            };
            if !deep {
                if let Some(redirect) = node.redirect {
                    return Some(if redirect.target == NodeId::ROOT {
                        format!("{here} ...")
                    } else {
                        format!("{here} -> {}", reg.nodes[redirect.target.0].usage())
                    });
                }
                let children: Vec<_> = node
                    .children
                    .iter()
                    .copied()
                    .filter(|id| reg.nodes[id.0].can_use(source))
                    .collect();
                let child_optional = node.executable();
                if let [only] = children.as_slice() {
                    if let Some(child) = usage(reg, *only, source, child_optional, child_optional) {
                        return Some(join(&here, &child));
                    }
                } else if !children.is_empty() {
                    let variants: BTreeSet<_> = children
                        .iter()
                        .filter_map(|&id| usage(reg, id, source, child_optional, true))
                        .collect();
                    if variants.len() == 1 {
                        let child = variants.first().expect("one variant");
                        return Some(join(
                            &here,
                            &if child_optional {
                                format!("[{child}]")
                            } else {
                                child.clone()
                            },
                        ));
                    }
                    let names = children
                        .iter()
                        .map(|id| reg.nodes[id.0].usage())
                        .collect::<Vec<_>>()
                        .join("|");
                    return Some(join(
                        &here,
                        &if child_optional {
                            format!("[{names}]")
                        } else {
                            format!("({names})")
                        },
                    ));
                }
            }
            Some(here)
        }
        let Some(root) = self.node(start) else {
            return vec![];
        };
        root.children
            .iter()
            .filter_map(|&id| usage(self, id, source, root.executable(), false).map(|s| (id, s)))
            .collect()
    }
}
fn join(left: &str, right: &str) -> String {
    if left.is_empty() {
        right.into()
    } else {
        format!("{left} {right}")
    }
}
