//! Tool ids and usage detail are additive: a consumer written before them still
//! compiles and sees the same events, and logs from before them still parse.

use mis_agent::claude::Parser;
use mis_types::{AgentEvent, TokenUsage};

/// A consumer predating them: it destructures the old variants' fields exactly
/// and skips the rest.
fn mirror(event: AgentEvent) -> Option<String> {
    Some(match event {
        AgentEvent::ToolUse { name, input } => format!("use {name} {input}"),
        AgentEvent::ToolResult { content } => format!("result {content}"),
        AgentEvent::Usage { input, output } => format!("usage {input} {output}"),
        AgentEvent::Finished { ok, summary } => format!("finished {ok} {summary:?}"),
        _ => return None,
    })
}

#[test]
fn a_consumer_predating_tool_ids_compiles_and_still_sees_every_call() {
    let mut p = Parser::default();
    let fixture = include_str!("fixtures/claude-subagent.jsonl");
    let got: Vec<String> = fixture
        .lines()
        .flat_map(|l| p.line(l))
        .filter_map(mirror)
        .collect();
    assert_eq!(got.iter().filter(|e| e.starts_with("use ")).count(), 3);
    assert_eq!(got.iter().filter(|e| e.starts_with("result ")).count(), 3);
    assert!(got.last().unwrap().starts_with("finished true"));
}

#[test]
fn logs_from_before_tool_ids_still_parse_and_new_events_need_only_their_ids() {
    let old = [
        r#"{"type":"tool_use","name":"Bash","input":{"command":"ls"}}"#,
        r#"{"type":"tool_result","content":"a"}"#,
        r#"{"type":"usage","input":110,"output":4}"#,
        r#"{"type":"finished","ok":true,"summary":null}"#,
    ];
    for line in old {
        let e: AgentEvent = serde_json::from_str(line).unwrap();
        assert!(mirror(e).is_some(), "{line}");
    }
    let new = |line: &str| serde_json::from_str::<AgentEvent>(line).unwrap();
    assert_eq!(
        new(r#"{"type":"tool_use_meta","id":"t1"}"#),
        AgentEvent::ToolUseMeta {
            id: "t1".into(),
            parent_id: None
        }
    );
    assert_eq!(
        new(r#"{"type":"tool_result_meta","id":"t1"}"#),
        AgentEvent::ToolResultMeta {
            id: "t1".into(),
            is_error: None
        }
    );
    let AgentEvent::UsageDetail { total, last, .. } =
        new(r#"{"type":"usage_detail","total":{"input":3}}"#)
    else {
        panic!()
    };
    assert_eq!((total.prompt(), last), (3, None));
    assert_eq!(
        total,
        TokenUsage {
            input: 3,
            ..Default::default()
        }
    );
}
