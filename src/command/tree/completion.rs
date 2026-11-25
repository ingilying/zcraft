use super::*;
use std::{future::poll_fn, task::Poll};

impl ParseResults<'_> {
    pub fn suggestions(&self, cursor: usize) -> Vec<Suggestion> {
        let input = self.completion_input(cursor);
        let mut result = Vec::new();
        self.collect_suggestions(&input, &mut result, false);
        merge_suggestions(&input, result)
    }

    /// Poll independent providers concurrently. Failed providers contribute no
    /// matches; dropping the caller future cancels the remaining providers.
    pub async fn suggestions_async(&self, cursor: usize) -> Vec<Suggestion> {
        let input = self.completion_input(cursor);
        let mut result = Vec::new();
        let mut tasks = self.collect_suggestions(&input, &mut result, true);
        poll_fn(|cx| {
            let mut pending = false;
            for task in &mut tasks {
                let Some(future) = task else { continue };
                match future.as_mut().poll(cx) {
                    Poll::Ready(outcome) => {
                        *task = None;
                        if let Ok(matches) = outcome {
                            extend_suggestions(&input, &mut result, matches);
                        }
                    }
                    Poll::Pending => pending = true,
                }
                if result.len() == MAX_SUGGESTIONS {
                    return Poll::Ready(());
                }
            }
            if pending {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        merge_suggestions(&input, result)
    }

    fn completion_input(&self, cursor: usize) -> Vec<u16> {
        let mut units = self.input.encode_utf16();
        let mut input: Vec<_> = units.by_ref().take(cursor).collect();
        if units.next().is_some_and(low_surrogate) {
            input.pop();
        }
        input
    }

    fn collect_suggestions<'a>(
        &'a self,
        input: &'a [u16],
        result: &mut Vec<Suggestion>,
        asynchronous: bool,
    ) -> Vec<Option<SuggestionFuture<'a>>> {
        let cursor = input.len();
        let mut parent = NodeId::ROOT;
        let mut start = self.contexts()[0].start.min(cursor);
        let mut arguments = &self.contexts()[0].arguments;
        'contexts: for context in self.contexts() {
            if cursor < context.start {
                break;
            }
            arguments = &context.arguments;
            parent = context.root;
            start = context.start;
            for node in &context.nodes {
                if cursor <= node.span.end {
                    start = node.span.start.min(cursor);
                    break 'contexts;
                }
                parent = node.id;
                start = (node.span.end + 1).min(cursor);
            }
        }
        let context = SuggestionContext {
            source: &self.source,
            arguments,
            input,
            start,
        };
        let prefix = context.remaining().to_lowercase();
        let mut tasks = Vec::new();
        for &child in &self.registry.nodes[parent.0].children {
            if result.len() == MAX_SUGGESTIONS {
                break;
            }
            let node = &self.registry.nodes[child.0];
            if !node.can_use(&self.source) {
                continue;
            }
            let mut add = |text: &str| {
                if text.to_lowercase().starts_with(&prefix) && text != context.remaining() {
                    extend_suggestions(input, result, [context.suggest(text)]);
                }
            };
            match &node.kind {
                NodeKind::Literal => add(&node.name),
                NodeKind::Argument(ArgumentKind::Bool) if !node.has_suggestion_provider() => {
                    add("true");
                    add("false");
                }
                _ => {
                    if let Some(provider) = node.async_suggestions.filter(|_| asynchronous) {
                        tasks.push(Some(provider(context)));
                    } else if let Some(provider) = node.suggestions {
                        extend_suggestions(input, result, provider(&context));
                    }
                }
            }
        }
        tasks
    }
}

fn low_surrogate(unit: u16) -> bool {
    (0xdc00..=0xdfff).contains(&unit)
}

fn extend_suggestions(
    input: &[u16],
    result: &mut Vec<Suggestion>,
    matches: impl IntoIterator<Item = Suggestion>,
) {
    let boundary = |i| i <= input.len() && !input.get(i).copied().is_some_and(low_surrogate);
    for candidate in matches.into_iter().take(MAX_SUGGESTIONS) {
        if result.len() == MAX_SUGGESTIONS {
            break;
        }
        if candidate.span.start <= candidate.span.end
            && boundary(candidate.span.start)
            && boundary(candidate.span.end)
            && candidate.text.len() <= MAX_SUGGESTION_BYTES
            && candidate
                .tooltip
                .as_ref()
                .is_none_or(|text| text.len() <= MAX_SUGGESTION_BYTES)
            && !result.contains(&candidate)
        {
            result.push(candidate);
        }
    }
}

fn merge_suggestions(input: &[u16], mut result: Vec<Suggestion>) -> Vec<Suggestion> {
    if let Some(first) = result.first() {
        let bounds = result.iter().fold(first.span.clone(), |r, s| {
            r.start.min(s.span.start)..r.end.max(s.span.end)
        });
        result.retain_mut(|suggestion| {
            if suggestion.span != bounds {
                if bounds.len().saturating_sub(suggestion.span.len()) + suggestion.text.len()
                    > MAX_SUGGESTION_BYTES
                {
                    return false;
                }
                suggestion.text = format!(
                    "{}{}{}",
                    String::from_utf16_lossy(&input[bounds.start..suggestion.span.start]),
                    suggestion.text,
                    String::from_utf16_lossy(&input[suggestion.span.end..bounds.end])
                );
                suggestion.span = bounds.clone();
            }
            suggestion.text.len() <= MAX_SUGGESTION_BYTES
        });
    }
    let mut unique = HashSet::new();
    result.retain(|s| unique.insert(s.clone()));
    result.sort_by(|a, b| {
        a.text
            .to_lowercase()
            .cmp(&b.text.to_lowercase())
            .then(a.text.cmp(&b.text))
            .then(a.tooltip.cmp(&b.tooltip))
    });
    result
}
