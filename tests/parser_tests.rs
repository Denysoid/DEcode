use decode::parser::{LivePreview, ParserEvent, ToolAction, parse_turn, visible_assistant_text};

fn parsed_actions(events: &[ParserEvent]) -> Vec<ToolAction> {
    events
        .iter()
        .filter_map(|event| match event {
            ParserEvent::ToolCallParsed(action) => Some(action.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn parses_all_six_tools_in_source_order() {
    let source = concat!(
        "<read_file><path>src/main.rs</path></read_file>",
        "<list_directory><path>src</path></list_directory>",
        "<search_code>",
        "<pattern>unsafe</pattern>",
        "<path>src</path>",
        "</search_code>",
        "<apply_patch>",
        "<path>src/lib.rs</path>",
        "<search>pub fn old() {}</search>",
        "<replace>pub fn new() {}</replace>",
        "</apply_patch>",
        "<write_file>",
        "<path>README.md</path>",
        "<content># Project</content>",
        "</write_file>",
        "<execute_command>",
        "<command>cargo check</command>",
        "<requires_confirmation>false</requires_confirmation>",
        "</execute_command>"
    );

    let actions = parsed_actions(&parse_turn(source));

    assert_eq!(
        actions,
        vec![
            ToolAction::ReadFile {
                path: "src/main.rs".to_owned(),
            },
            ToolAction::ListDirectory {
                path: "src".to_owned(),
            },
            ToolAction::SearchCode {
                pattern: "unsafe".to_owned(),
                path: Some("src".to_owned()),
            },
            ToolAction::ApplyPatch {
                path: "src/lib.rs".to_owned(),
                search: "pub fn old() {}".to_owned(),
                replace: "pub fn new() {}".to_owned(),
            },
            ToolAction::WriteFile {
                path: "README.md".to_owned(),
                content: "# Project".to_owned(),
            },
            ToolAction::ExecuteCommand {
                command: "cargo check".to_owned(),
                requires_confirmation: false,
            },
        ]
    );
}

#[test]
fn accepts_fields_in_different_orders() {
    let source = concat!(
        "<search_code>",
        "<path>src</path>",
        "<pattern>unsafe</pattern>",
        "</search_code>",
        "<apply_patch>",
        "<replace>new</replace>",
        "<search>old</search>",
        "<path>src/lib.rs</path>",
        "</apply_patch>",
        "<execute_command>",
        "<requires_confirmation>false</requires_confirmation>",
        "<command>cargo check</command>",
        "</execute_command>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![
            ToolAction::SearchCode {
                pattern: "unsafe".to_owned(),
                path: Some("src".to_owned()),
            },
            ToolAction::ApplyPatch {
                path: "src/lib.rs".to_owned(),
                search: "old".to_owned(),
                replace: "new".to_owned(),
            },
            ToolAction::ExecuteCommand {
                command: "cargo check".to_owned(),
                requires_confirmation: false,
            },
        ]
    );
}

#[test]
fn thinking_does_not_create_tool_actions() {
    let source = concat!(
        "<thinking>",
        "Maybe use <read_file><path>secret</path></read_file>.",
        "</thinking>",
        "Final answer."
    );

    assert_eq!(
        parse_turn(source),
        vec![ParserEvent::TurnComplete {
            had_tool_calls: false,
        }]
    );
}

#[test]
fn preserves_generics_and_unescaped_code_characters() {
    let source = concat!(
        "<write_file>",
        "<path>src/lib.rs</path>",
        "<content>",
        "pub fn convert<T: Into<U>, U>(v: T) -> U { ",
        "let x = a < b && c > d; v.into() }",
        "</content>",
        "</write_file>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::WriteFile {
            path: "src/lib.rs".to_owned(),
            content: concat!(
                "pub fn convert<T: Into<U>, U>(v: T) -> U { ",
                "let x = a < b && c > d; v.into() }"
            )
            .to_owned(),
        }]
    );
}

#[test]
fn decodes_standard_xml_entities_in_shell_commands() {
    let source = concat!(
        "<execute_command>",
        "<command>py -3 -c &quot;print('ok')&quot; &amp;&amp; echo done</command>",
        "</execute_command>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::ExecuteCommand {
            command: "py -3 -c \"print('ok')\" && echo done".to_owned(),
            requires_confirmation: true,
        }]
    );
}

#[test]
fn preserves_crlf_inside_fields() {
    let source = concat!(
        "<write_file>\r\n",
        "<path>file.txt</path>\r\n",
        "<content>a\r\nb\r\n</content>\r\n",
        "</write_file>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::WriteFile {
            path: "file.txt".to_owned(),
            content: "a\r\nb\r\n".to_owned(),
        }]
    );
}

#[test]
fn supports_empty_replace_for_deletion() {
    let source = concat!(
        "<apply_patch>",
        "<path>src/lib.rs</path>",
        "<search>obsolete</search>",
        "<replace></replace>",
        "</apply_patch>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::ApplyPatch {
            path: "src/lib.rs".to_owned(),
            search: "obsolete".to_owned(),
            replace: String::new(),
        }]
    );
}

#[test]
fn supports_empty_write_content() {
    let source = concat!(
        "<write_file>",
        "<path>empty.txt</path>",
        "<content></content>",
        "</write_file>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::WriteFile {
            path: "empty.txt".to_owned(),
            content: String::new(),
        }]
    );
}

#[test]
fn partial_success_keeps_later_valid_action() {
    let source = concat!(
        "<apply_patch>",
        "<path>src/lib.rs</path>",
        "<search>old</search>",
        "</apply_patch>",
        "<read_file><path>src/lib.rs</path></read_file>"
    );

    let events = parse_turn(source);

    assert!(
        events
            .iter()
            .any(|event| { matches!(event, ParserEvent::ToolCallParseError { .. }) })
    );

    assert_eq!(
        parsed_actions(&events),
        vec![ToolAction::ReadFile {
            path: "src/lib.rs".to_owned(),
        }]
    );

    assert!(events.iter().any(|event| {
        matches!(
            event,
            ParserEvent::TurnComplete {
                had_tool_calls: true
            }
        )
    }));
}

#[test]
fn parse_error_contains_tool_block_number() {
    let source = concat!(
        "<read_file><path>a.rs</path></read_file>",
        "<apply_patch><path>b.rs</path></apply_patch>"
    );

    let events = parse_turn(source);

    assert!(events.iter().any(|event| {
        matches!(
            event,
            ParserEvent::ToolCallParseError { reason, .. }
                if reason.contains("block #2")
        )
    }));
}

#[test]
fn unclosed_outer_tool_is_never_executed() {
    let source = "<write_file><path>x</path><content>partial";
    let events = parse_turn(source);

    assert!(parsed_actions(&events).is_empty());

    assert!(
        events
            .iter()
            .any(|event| { matches!(event, ParserEvent::ToolCallParseError { .. }) })
    );
}

#[test]
fn rebuilt_split_tool_tag_is_parsed_authoritatively() {
    let chunks = ["<read_fi", "le><path>src/", "lib.rs</path></read_file>"];

    let full_response = chunks.concat();

    assert_eq!(
        parsed_actions(&parse_turn(&full_response)),
        vec![ToolAction::ReadFile {
            path: "src/lib.rs".to_owned(),
        }]
    );
}

#[test]
fn unknown_outer_tool_is_not_executed() {
    let source = "<delete_file><path>src/lib.rs</path></delete_file>";
    let events = parse_turn(source);

    assert!(parsed_actions(&events).is_empty());

    assert!(events.iter().any(|event| {
        matches!(
            event,
            ParserEvent::TurnComplete {
                had_tool_calls: false
            }
        )
    }));
}

#[test]
fn foreign_text_between_tools_is_nonfatal() {
    let source = concat!(
        "before",
        "<read_file><path>a.rs</path></read_file>",
        "between",
        "<read_file><path>b.rs</path></read_file>",
        "after"
    );

    assert_eq!(parsed_actions(&parse_turn(source)).len(), 2);
}

#[test]
fn duplicate_required_field_is_rejected() {
    let source = concat!(
        "<read_file>",
        "<path>a.rs</path>",
        "<path>b.rs</path>",
        "</read_file>"
    );

    let events = parse_turn(source);

    assert!(parsed_actions(&events).is_empty());

    assert!(events.iter().any(|event| {
        matches!(
            event,
            ParserEvent::ToolCallParseError { reason, .. }
                if reason.contains("more than once")
        )
    }));
}

#[test]
fn nested_known_field_is_rejected() {
    let source = concat!(
        "<apply_patch>",
        "<path>src/lib.rs</path>",
        "<search>before <replace>evil</replace> after</search>",
        "<replace>good</replace>",
        "</apply_patch>"
    );

    let events = parse_turn(source);

    assert!(parsed_actions(&events).is_empty());

    assert!(
        events
            .iter()
            .any(|event| { matches!(event, ParserEvent::ToolCallParseError { .. }) })
    );
}

#[test]
fn literal_search_closing_tag_produces_error() {
    let source = concat!(
        "<apply_patch>",
        "<path>src/lib.rs</path>",
        "<search>let marker = \"</search>\";</search>",
        "<replace>replacement</replace>",
        "</apply_patch>"
    );

    let events = parse_turn(source);

    assert!(parsed_actions(&events).is_empty());

    assert!(
        events
            .iter()
            .any(|event| { matches!(event, ParserEvent::ToolCallParseError { .. }) })
    );
}

#[test]
fn missing_confirmation_defaults_to_true() {
    let source = concat!(
        "<execute_command>",
        "<command>cargo test</command>",
        "</execute_command>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::ExecuteCommand {
            command: "cargo test".to_owned(),
            requires_confirmation: true,
        }]
    );
}

#[test]
fn invalid_confirmation_value_defaults_to_true() {
    let source = concat!(
        "<execute_command>",
        "<command>cargo test</command>",
        "<requires_confirmation>FALSE</requires_confirmation>",
        "</execute_command>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::ExecuteCommand {
            command: "cargo test".to_owned(),
            requires_confirmation: true,
        }]
    );
}

#[test]
fn unclosed_confirmation_defaults_to_true() {
    let source = concat!(
        "<execute_command>",
        "<command>cargo test</command>",
        "<requires_confirmation>false",
        "</execute_command>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::ExecuteCommand {
            command: "cargo test".to_owned(),
            requires_confirmation: true,
        }]
    );
}

#[test]
fn duplicate_confirmation_defaults_to_true() {
    let source = concat!(
        "<execute_command>",
        "<command>cargo test</command>",
        "<requires_confirmation>false</requires_confirmation>",
        "<requires_confirmation>false</requires_confirmation>",
        "</execute_command>"
    );

    assert_eq!(
        parsed_actions(&parse_turn(source)),
        vec![ToolAction::ExecuteCommand {
            command: "cargo test".to_owned(),
            requires_confirmation: true,
        }]
    );
}

#[test]
fn unrelated_sibling_in_execute_command_is_rejected() {
    let source = concat!(
        "<execute_command>",
        "<command>cargo test</command>",
        "<unexpected>false</unexpected>",
        "</execute_command>"
    );

    let events = parse_turn(source);

    assert!(parsed_actions(&events).is_empty());

    assert!(
        events
            .iter()
            .any(|event| { matches!(event, ParserEvent::ToolCallParseError { .. }) })
    );
}

#[test]
fn malformed_confirmation_cannot_swallow_unrelated_field() {
    let source = concat!(
        "<execute_command>",
        "<command>cargo test</command>",
        "<requires_confirmation_evil>false",
        "<path>outside</path>",
        "</requires_confirmation_evil>",
        "</execute_command>"
    );

    let events = parse_turn(source);

    assert!(parsed_actions(&events).is_empty());
}

#[test]
fn live_preview_handles_markers_split_across_chunks() {
    let mut preview = LivePreview::new();
    let mut events = Vec::new();

    events.extend(preview.feed("outside<thi"));
    events.extend(preview.feed("nking>Hello <Vec<T>>"));
    events.extend(preview.feed(" world</thin"));
    events.extend(preview.feed("king>outside"));

    assert_eq!(
        events,
        vec![
            ParserEvent::ThinkingDelta("Hello <Vec<T>>".to_owned()),
            ParserEvent::ThinkingDelta(" world".to_owned()),
            ParserEvent::ThinkingEnd,
        ]
    );

    assert!(!preview.is_inside_thinking());
}

#[test]
fn live_preview_handles_multiple_thinking_blocks() {
    let mut preview = LivePreview::new();

    assert_eq!(
        preview.feed(concat!(
            "<thinking>one</thinking>",
            "outside",
            "<thinking>two</thinking>"
        )),
        vec![
            ParserEvent::ThinkingDelta("one".to_owned()),
            ParserEvent::ThinkingEnd,
            ParserEvent::ThinkingDelta("two".to_owned()),
            ParserEvent::ThinkingEnd,
        ]
    );
}

#[test]
fn live_preview_never_parses_tools() {
    let mut preview = LivePreview::new();

    let events = preview.feed(concat!(
        "<thinking>",
        "<apply_patch><path>x</path><search>a</search>",
        "<replace>b</replace></apply_patch>",
        "</thinking>"
    ));

    assert!(events.iter().all(|event| {
        matches!(
            event,
            ParserEvent::ThinkingDelta(_) | ParserEvent::ThinkingEnd
        )
    }));
}

#[test]
fn live_preview_finish_does_not_fake_thinking_end() {
    let mut preview = LivePreview::new();
    let mut events = preview.feed("<thinking>unfinished</think");

    events.extend(preview.finish());

    assert_eq!(
        events,
        vec![
            ParserEvent::ThinkingDelta("unfinished".to_owned()),
            ParserEvent::ThinkingDelta("</think".to_owned()),
        ]
    );

    assert!(!preview.is_inside_thinking());
}

#[test]
fn live_preview_handles_unicode_at_chunk_boundaries() {
    let mut preview = LivePreview::new();
    let mut events = preview.feed("Привет <think");

    events.extend(preview.feed("ing>мир 🌍</thinking> конец"));

    assert_eq!(
        events,
        vec![
            ParserEvent::ThinkingDelta("мир 🌍".to_owned()),
            ParserEvent::ThinkingEnd,
        ]
    );
}

#[test]
fn tool_action_deserialization_rejects_unknown_fields() {
    let json = concat!(
        "{",
        "\"type\":\"read_file\",",
        "\"path\":\"src/lib.rs\",",
        "\"unexpected\":true",
        "}"
    );

    assert!(serde_json::from_str::<ToolAction>(json).is_err());
}

#[test]
fn tool_action_deserialization_rejects_unknown_tool() {
    let json = "{\"type\":\"delete_file\",\"path\":\"src/lib.rs\"}";

    assert!(serde_json::from_str::<ToolAction>(json).is_err());
}

#[test]
fn parser_event_deserialization_rejects_unknown_fields() {
    let json = concat!(
        "{",
        "\"type\":\"turn_complete\",",
        "\"payload\":{",
        "\"had_tool_calls\":false,",
        "\"unexpected\":true",
        "}",
        "}"
    );

    assert!(serde_json::from_str::<ParserEvent>(json).is_err());
}

#[test]
fn visible_text_excludes_thinking_and_tool_protocol() {
    let source = concat!(
        "<thinking>secret chain</thinking>",
        "Проверяю файл.",
        "<read_file><path>src/lib.rs</path></read_file>",
        " Готово."
    );

    assert_eq!(visible_assistant_text(source), "Проверяю файл. Готово.");
}
