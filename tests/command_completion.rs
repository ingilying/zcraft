use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};
use zcraft::command::{
    ArgumentKind, CommandNode, CommandRegistryBuilder, CommandSource, MAX_SUGGESTIONS, NodeId,
    Suggestion, SuggestionContext, SuggestionFuture,
};

#[tokio::test]
async fn independent_providers_are_polled_together_and_bad_results_are_ignored() {
    static OTHER_POLLED: AtomicBool = AtomicBool::new(false);
    fn waiting(ctx: SuggestionContext<'_>) -> SuggestionFuture<'_> {
        Box::pin(async move {
            while !OTHER_POLLED.load(Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
            Ok(vec![
                ctx.suggest("good"),
                Suggestion {
                    span: 1..2,
                    text: "split surrogate".into(),
                    tooltip: None,
                },
                Suggestion {
                    span: usize::MAX..usize::MAX,
                    text: "outside".into(),
                    tooltip: None,
                },
                Suggestion {
                    span: ctx.start..ctx.start,
                    text: "too large".repeat(1000),
                    tooltip: None,
                },
            ])
        })
    }
    fn broken(_: SuggestionContext<'_>) -> SuggestionFuture<'_> {
        Box::pin(async {
            OTHER_POLLED.store(true, Ordering::Relaxed);
            Err(io::Error::other("provider failed"))
        })
    }
    let mut builder = CommandRegistryBuilder::new();
    builder
        .add_node(
            NodeId::ROOT,
            CommandNode::argument("wait", ArgumentKind::Word)
                .suggests(|ctx| vec![ctx.suggest("fallback")])
                .suggests_async(waiting),
        )
        .unwrap();
    builder
        .add_node(
            NodeId::ROOT,
            CommandNode::argument("error", ArgumentKind::Word).suggests_async(broken),
        )
        .unwrap();
    builder
        .add_node(
            NodeId::ROOT,
            CommandNode::argument("hidden", ArgumentKind::Word)
                .requires(|s| s.permission > 0)
                .suggests(|_| panic!("hidden provider must not run")),
        )
        .unwrap();
    let registry = builder.build().unwrap();
    let parse = registry.dispatch("🙂", &CommandSource::player(uuid::Uuid::nil()));
    assert_eq!(
        parse.suggestions(1),
        [Suggestion {
            span: 0..0,
            text: "fallback".into(),
            tooltip: None
        }]
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        parse.suggestions_async(2),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        [Suggestion {
            span: 0..2,
            text: "good".into(),
            tooltip: None
        }]
    );
}

#[test]
fn sync_provider_output_is_bounded() {
    let mut builder = CommandRegistryBuilder::new();
    builder
        .add_node(
            NodeId::ROOT,
            CommandNode::argument("many", ArgumentKind::Word).suggests(|ctx| {
                (0..MAX_SUGGESTIONS * 2)
                    .map(|i| ctx.suggest(i.to_string()))
                    .collect()
            }),
        )
        .unwrap();
    let registry = builder.build().unwrap();
    let parse = registry.dispatch("", &CommandSource::player(uuid::Uuid::nil()));
    assert_eq!(parse.suggestions(0).len(), MAX_SUGGESTIONS);
}
