//! Bounded, synchronous Minecraft command execution, above the dispatcher.
use super::*;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct ExecutionLimits {
    /// One unit per queued dispatch, redirect application, or executor call.
    pub cost: usize,
    /// Total sources produced across redirects throughout this run.
    pub sources: usize,
    pub queued: usize,
    pub frame_depth: usize,
}
impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            cost: 128,
            sources: 64,
            queued: 64,
            frame_depth: 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionFailure {
    Cost,
    Sources,
    Queue,
    Depth,
    Cancelled,
    InvalidSource,
    InvalidCommand,
    Feedback,
    Host,
}
impl std::fmt::Display for ExecutionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cost => "Command chain budget exceeded.",
            Self::Sources => "Command source budget exceeded.",
            Self::Queue => "Command queue limit exceeded.",
            Self::Depth => "Command frame depth exceeded.",
            Self::Cancelled => "Command execution cancelled.",
            Self::InvalidSource => "Invalid command source.",
            Self::InvalidCommand => "Invalid command text.",
            Self::Feedback => "Command feedback limit exceeded.",
            Self::Host => "Command execution failed.",
        })
    }
}
impl std::error::Error for ExecutionFailure {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReport {
    /// Last action of the root frame, or the value explicitly returned to it.
    /// Feedback contains permitted output from all executed actions in order.
    pub result: CommandResult,
    /// Runtime abort, as opposed to an ordinary unsuccessful command result.
    pub abort: Option<ExecutionFailure>,
    pub steps: usize,
}

/// Narrow, nonrecursive execution control provided to trusted executors.
/// Enqueues run after this executor returns, sharing this run's budgets.
pub trait ExecutionControl {
    fn enqueue(&mut self, input: &str) -> Result<(), ExecutionFailure>;
    fn enqueue_frame(
        &mut self,
        input: &str,
        source: SourceStack,
        callback: Option<CommandCallback>,
    ) -> Result<(), ExecutionFailure>;
    /// Set the current frame's result and discard its remaining subtree.
    fn return_from_frame(&mut self, success: bool, value: i32);
    /// Abort the whole chain, not a rollback of already performed effects.
    fn cancel(&mut self);
}

struct Entry {
    frame: usize,
    input: String,
    source: SourceStack,
}
struct Frame {
    parent: Option<usize>,
    depth: usize,
    result: CommandResult,
    callback: Option<CommandCallback>,
    done: bool,
}
impl Frame {
    fn new(parent: Option<usize>, depth: usize, callback: Option<CommandCallback>) -> Self {
        Self {
            parent,
            depth,
            result: CommandResult::success(0),
            callback,
            done: false,
        }
    }
    fn complete(&mut self) {
        self.done = true;
        if let Some(reply) = self.callback.take() {
            reply(self.result.success, self.result.value);
        }
    }
}

pub(super) struct Runner {
    limits: ExecutionLimits,
    cost: usize,
    sources: usize,
    queue: VecDeque<Entry>,
    // Newly scheduled actions run in call order before older queued work.
    pending: Vec<Entry>,
    frames: Vec<Frame>,
    abort: Option<ExecutionFailure>,
    returned: Option<CommandResult>,
    feedback: Vec<String>,
    feedback_bytes: usize,
    completed_sources: usize,
}
impl Runner {
    fn fail(&mut self, failure: ExecutionFailure) -> ExecutionFailure {
        *self.abort.get_or_insert(failure)
    }
    pub(super) fn step(
        &mut self,
        host: &dyn CommandHost,
        source: &SourceStack,
    ) -> Result<(), ExecutionFailure> {
        if let Some(failure) = self.abort {
            return Err(failure);
        }
        if source.validate().is_err() {
            return Err(self.fail(ExecutionFailure::InvalidSource));
        }
        if !host.command_active(source) {
            return Err(self.fail(ExecutionFailure::Cancelled));
        }
        if self.cost >= self.limits.cost {
            return Err(self.fail(ExecutionFailure::Cost));
        }
        self.cost += 1;
        Ok(())
    }
    pub(super) fn expand(&mut self, count: usize) -> Result<(), ExecutionFailure> {
        if count > self.limits.sources - self.sources {
            return Err(self.fail(ExecutionFailure::Sources));
        }
        self.sources += count;
        Ok(())
    }
    pub(super) fn returned(&self) -> bool {
        self.returned.is_some()
    }
    pub(super) fn complete_source(&mut self, source: &SourceStack, success: bool, value: i32) {
        self.completed_sources += 1;
        source.complete(success, value);
    }
    pub(super) fn complete_execution(&mut self, source: &SourceStack, result: &CommandResult) {
        let result = self.returned.as_ref().unwrap_or(result);
        let (success, value) = (result.success, result.value);
        self.complete_source(source, success, value);
    }
    fn push(
        &mut self,
        frame: usize,
        input: &str,
        source: SourceStack,
    ) -> Result<(), ExecutionFailure> {
        if let Some(failure) = self.abort {
            return Err(failure);
        }
        if self.returned.is_some() {
            return Err(ExecutionFailure::Cancelled);
        }
        if self.queue.len() + self.pending.len() >= self.limits.queued {
            return Err(self.fail(ExecutionFailure::Queue));
        }
        if input.len() > MAX_COMMAND_BYTES {
            return Err(self.fail(ExecutionFailure::InvalidCommand));
        }
        if source.validate().is_err() {
            return Err(self.fail(ExecutionFailure::InvalidSource));
        }
        self.pending.push(Entry {
            frame,
            input: input.into(),
            source,
        });
        Ok(())
    }
    fn flush_pending(&mut self) {
        for entry in self.pending.drain(..).rev() {
            self.queue.push_front(entry);
        }
    }

    pub(super) fn feedback(
        &mut self,
        source: &SourceStack,
        feedback: Vec<String>,
    ) -> Result<(), ExecutionFailure> {
        if source.feedback_destination() == FeedbackDestination::Discard {
            return Ok(());
        }
        for line in feedback {
            self.feedback_bytes = self.feedback_bytes.saturating_add(line.len());
            if self.feedback.len() == 64 || self.feedback_bytes > 16 * 1024 {
                return Err(self.fail(ExecutionFailure::Feedback));
            }
            self.feedback.push(line);
        }
        Ok(())
    }
    pub(super) fn invoke(
        &mut self,
        frame: usize,
        source: &SourceStack,
        host: &mut dyn CommandHost,
        execute: Executor,
        call: &Invocation<'_>,
    ) -> Result<CommandResult, ExecutionFailure> {
        self.step(host, source)?;
        let mut adapter = RuntimeHost {
            runner: self,
            frame,
            source,
            host,
        };
        let result = execute(call, &mut adapter).map_err(|error| {
            tracing::warn!(%error, "command executor failed");
            adapter.runner.fail(ExecutionFailure::Host)
        })?;
        if !adapter.host.command_active(source) {
            return Err(adapter.runner.fail(ExecutionFailure::Cancelled));
        }
        if let Some(failure) = adapter.runner.abort {
            return Err(failure);
        }
        Ok(result)
    }
    fn descendant_of(frames: &[Frame], mut id: usize, ancestor: usize) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            let Some(parent) = frames[id].parent else {
                return false;
            };
            id = parent;
        }
    }
    fn discard_descendants(&mut self, id: usize) {
        let frames = &self.frames;
        self.queue
            .retain(|entry| !Self::descendant_of(frames, entry.frame, id));
        for child in (id + 1..self.frames.len()).rev() {
            if !self.frames[child].done && Self::descendant_of(&self.frames, child, id) {
                self.frames[child].result = CommandResult::failure("");
                self.frames[child].complete();
            }
        }
        self.frames[id].complete();
    }
    fn finish_idle_frames(&mut self, mut id: usize) {
        loop {
            if self
                .queue
                .iter()
                .any(|entry| Self::descendant_of(&self.frames, entry.frame, id))
            {
                break;
            }
            if !self.frames[id].done {
                self.frames[id].complete();
            }
            let Some(parent) = self.frames[id].parent else {
                break;
            };
            id = parent;
        }
    }
}

struct RuntimeHost<'a> {
    runner: &'a mut Runner,
    frame: usize,
    source: &'a SourceStack,
    host: &'a mut dyn CommandHost,
}
impl RuntimeHost<'_> {
    fn can_send_chat(&self, player: Uuid) -> bool {
        self.source.authenticated_player() == Some(player)
            && self.runner.abort.is_none()
            && !self.runner.returned()
            && self.host.command_active(self.source)
    }
}
impl CommandHost for RuntimeHost<'_> {
    fn list_players(&mut self) -> io::Result<CommandResult> {
        self.host.list_players()
    }
    fn action(&mut self, player: Uuid, text: &str) -> io::Result<CommandResult> {
        if !self.can_send_chat(player) {
            return Ok(CommandResult::failure("Player unavailable."));
        }
        self.host.action(player, text)
    }
    fn whisper(&mut self, player: Uuid, target: &str, text: &str) -> io::Result<CommandResult> {
        if !self.can_send_chat(player) {
            return Ok(CommandResult::failure("Player unavailable."));
        }
        self.host.whisper(player, target, text)
    }
    fn command_entities(&mut self) -> io::Result<Vec<arguments::EntitySnapshot>> {
        if !self.host.command_active(self.source) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Command source is no longer active.",
            ));
        }
        self.host.command_entities()
    }
    fn command_active(&self, source: &SourceStack) -> bool {
        self.host.command_active(source)
    }
    fn execution(&mut self) -> Option<&mut dyn ExecutionControl> {
        Some(self)
    }
    fn world(&mut self) -> Option<&mut dyn WorldCommandHost> {
        self.host.world()
    }
    fn admin(&mut self) -> Option<&mut dyn AdminCommandHost> {
        self.host.admin()
    }
}
impl ExecutionControl for RuntimeHost<'_> {
    fn enqueue(&mut self, input: &str) -> Result<(), ExecutionFailure> {
        self.runner.push(self.frame, input, self.source.clone())
    }
    fn enqueue_frame(
        &mut self,
        input: &str,
        source: SourceStack,
        callback: Option<CommandCallback>,
    ) -> Result<(), ExecutionFailure> {
        let depth = self.runner.frames[self.frame].depth + 1;
        if depth > self.runner.limits.frame_depth {
            return Err(self.runner.fail(ExecutionFailure::Depth));
        }
        let frame = self.runner.frames.len();
        self.runner
            .push(frame, input, source.inherit_authority(self.source))?;
        self.runner
            .frames
            .push(Frame::new(Some(self.frame), depth, callback));
        Ok(())
    }
    fn return_from_frame(&mut self, success: bool, value: i32) {
        self.runner.returned.get_or_insert_with(|| CommandResult {
            success,
            value: if success { value } else { 0 },
            feedback: vec![],
        });
    }
    fn cancel(&mut self) {
        self.runner.fail(ExecutionFailure::Cancelled);
    }
}

impl CommandRegistry {
    /// Drain one bounded chain synchronously. The host may check cancellation
    /// or player readiness between steps. Callbacks and executors must not
    /// block or invoke a fresh runtime recursively.
    pub fn run(
        &self,
        input: &str,
        source: &SourceStack,
        host: &mut dyn CommandHost,
        limits: ExecutionLimits,
    ) -> ExecutionReport {
        if host.execution().is_some() {
            source.complete(false, 0);
            return ExecutionReport {
                result: CommandResult::failure(
                    "Enqueue commands instead of starting a nested runtime.",
                ),
                abort: Some(ExecutionFailure::Host),
                steps: 0,
            };
        }
        let mut runner = Runner {
            limits,
            cost: 0,
            sources: 0,
            queue: VecDeque::new(),
            pending: Vec::new(),
            frames: vec![Frame::new(None, 0, None)],
            abort: None,
            returned: None,
            feedback: Vec::new(),
            feedback_bytes: 0,
            completed_sources: 0,
        };
        if runner.push(0, input, source.clone()).is_err() {
            source.complete(false, 0);
        }
        runner.flush_pending();
        while runner.abort.is_none() {
            let Some(entry) = runner.queue.pop_front() else {
                break;
            };
            let completed_before = runner.completed_sources;
            let result = runner
                .step(host, &entry.source)
                .and_then(
                    |()| match self.server_parse(&entry.input, entry.source.source()) {
                        Ok(parsed) => {
                            parsed.execute_runtime(&entry.source, host, &mut runner, entry.frame)
                        }
                        Err(result) => Ok(result),
                    },
                )
                .and_then(|mut result| {
                    runner.feedback(&entry.source, std::mem::take(&mut result.feedback))?;
                    if !result.success {
                        result.value = 0;
                    }
                    Ok(runner
                        .returned
                        .take()
                        .map_or((result, false), |value| (value, true)))
                });
            runner.flush_pending();
            match result {
                Ok((result, returned)) => {
                    if runner.completed_sources == completed_before {
                        runner.complete_source(&entry.source, result.success, result.value);
                    }
                    runner.frames[entry.frame].result = result;
                    if returned {
                        runner.discard_descendants(entry.frame);
                    }
                    runner.finish_idle_frames(entry.frame);
                }
                Err(failure) => {
                    runner.fail(failure);
                    if runner.completed_sources == completed_before {
                        runner.complete_source(&entry.source, false, 0);
                    }
                }
            }
        }
        if runner.abort.is_none() && !host.command_active(source) {
            runner.fail(ExecutionFailure::Cancelled);
        }
        let result = if let Some(failure) = runner.abort {
            for frame in runner.frames.iter_mut().rev().filter(|frame| !frame.done) {
                frame.result = CommandResult::failure("");
                frame.complete();
            }
            let mut result = CommandResult::failure(failure.to_string());
            if failure == ExecutionFailure::Cancelled
                || source.feedback_destination() == FeedbackDestination::Discard
                || !host.command_active(source)
            {
                result.feedback.clear();
            }
            result
        } else {
            let mut result =
                std::mem::replace(&mut runner.frames[0].result, CommandResult::success(0));
            result.feedback = runner.feedback;
            result
        };
        ExecutionReport {
            result,
            abort: runner.abort,
            steps: runner.cost,
        }
    }
}
