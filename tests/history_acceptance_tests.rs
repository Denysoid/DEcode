use decode::{
    agent::{AgentState, HistoryKind},
    parser::{ToolAction, ToolOutcome},
};

#[test]
fn reported_usage_triggers_tool_result_compaction_before_approximate_fallback() {
    let mut state = AgentState::new();
    state.push_user(1, "anchor");
    let action = ToolAction::ReadFile {
        path: "large.txt".to_owned(),
    };
    state.push_tool_result_with_action(1, 7, &action, &ToolOutcome::success("x".repeat(600)));
    state.push_user(2, "newest prompt");

    let approximate_only = state.compacted_committed(800);
    assert!(
        approximate_only
            .iter()
            .any(|entry| entry.content == "x".repeat(600))
    );

    state.record_usage(1_000, state.last_committed_sequence());
    let usage_driven = state.compacted_committed(800);
    let placeholder = usage_driven
        .iter()
        .find(|entry| matches!(entry.kind, HistoryKind::ToolResult { .. }))
        .expect("tool result disappeared instead of being compacted");
    assert!(placeholder.content.contains("tool=read_file"));
    assert!(placeholder.content.contains("action_id=7"));
    assert!(placeholder.content.contains("path=large.txt"));
    assert!(placeholder.content.contains("sha256="));
}

#[test]
fn newest_assistant_is_never_retained_without_its_causal_tool_round() {
    let mut state = AgentState::new();
    state.push_user(1, "anchor");
    state.push_user(2, "latest request");
    state.push_assistant(2, "<read_file><path>large.txt</path></read_file>".repeat(8));
    state.push_tool_result_with_action(
        2,
        9,
        &ToolAction::ReadFile {
            path: "large.txt".to_owned(),
        },
        &ToolOutcome::success("x".repeat(600)),
    );
    state.push_assistant(2, "final answer");

    let compacted = state.compacted_committed(10);
    let final_is_kept = compacted
        .iter()
        .any(|entry| entry.turn_id == 2 && entry.content == "final answer");
    if final_is_kept {
        assert!(
            compacted
                .iter()
                .any(|entry| entry.turn_id == 2 && matches!(entry.kind, HistoryKind::User)),
            "newest assistant was orphaned from its user prompt"
        );
        assert!(
            compacted.iter().any(|entry| {
                entry.turn_id == 2 && matches!(entry.kind, HistoryKind::ToolResult { .. })
            }),
            "newest assistant was orphaned from its tool result"
        );
        assert!(
            compacted.iter().any(|entry| {
                entry.turn_id == 2
                    && matches!(entry.kind, HistoryKind::Assistant)
                    && entry.content.contains("<read_file>")
            }),
            "newest assistant was orphaned from the assistant tool call"
        );
    }
}
