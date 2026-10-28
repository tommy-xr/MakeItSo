//! Completion summaries: pure prompt construction and reply parsing. The shell
//! runs the model; nothing here does I/O.

use mis_types::AgentEvent;

/// Budget for the rendered task prompt plus transcript.
const MAX_CHARS: usize = 8_000;
/// Per-part caps, so one huge prompt or tool result can't crowd out the rest.
/// Agent text gets more: the final answer is the best evidence of the outcome.
const MAX_PROMPT_CHARS: usize = 2_000;
const MAX_TEXT_CHARS: usize = 2_000;
const MAX_EVENT_CHARS: usize = 600;
/// A final answer this short, with no tool calls, already is its own summary.
const SHORT_ANSWER_CHARS: usize = 300;

/// Whether a finished task is worth summarizing: tools ran, or the answer is long.
pub fn wanted(events: &[AgentEvent], summary: Option<&str>) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolUse { .. }))
        || summary.is_some_and(|s| s.chars().count() > SHORT_ANSWER_CHARS)
}

/// Prompt asking for 1-2 sentences over the task prompt and its latest events.
pub fn prompt(task_prompt: &str, events: &[AgentEvent]) -> String {
    let task_prompt = cut(task_prompt.trim(), MAX_PROMPT_CHARS);
    let budget = MAX_CHARS.saturating_sub(task_prompt.chars().count());
    format!(
        "Summarize a coding agent's finished task for someone glancing at a phone. Reply \
         with 1-2 plain sentences and nothing else (no preamble, no markdown, don't start \
         with \"The agent\"): what was done and the outcome, with any key names or numbers; \
         then anything the user must act on, only if there is something.\n\n\
         Task:\n{task_prompt}\n\nTranscript:\n{}",
        transcript(events, budget)
    )
}

/// The latest events as `mis show`-style lines within `budget` chars, oldest
/// first, led by `(earlier events omitted)` when some didn't fit.
pub fn transcript(events: &[AgentEvent], mut budget: usize) -> String {
    let mut lines: Vec<String> = vec![];
    let mut omitted = "";
    for line in events.iter().rev().filter_map(line) {
        let n = line.chars().count() + 1;
        if n > budget {
            omitted = "(earlier events omitted)\n";
            break;
        }
        budget -= n;
        lines.push(line);
    }
    lines.reverse();
    format!("{omitted}{}", lines.join("\n"))
}

/// One transcript line, like `mis show`; None for events with nothing to say.
/// `Finished` drops its summary: that repeats the agent's last text.
fn line(e: &AgentEvent) -> Option<String> {
    Some(match e {
        AgentEvent::Text { text } => cut(text.trim(), MAX_TEXT_CHARS),
        AgentEvent::ToolUse { name, input } => cut(&format!("> {name} {input}"), MAX_EVENT_CHARS),
        AgentEvent::ToolResult { content } => {
            cut(&format!("< {}", content.trim()), MAX_EVENT_CHARS)
        }
        AgentEvent::Status { text } => cut(&format!("status: {}", text.trim()), MAX_EVENT_CHARS),
        AgentEvent::Evidence(l) => cut(&format!("evidence: {}", l.line()), MAX_EVENT_CHARS),
        AgentEvent::Finished { ok, .. } => if *ok { "done" } else { "FAILED" }.into(),
        _ => return None,
    })
}

/// `s` cut to `max` chars, keeping the head and the tail (where results and
/// test verdicts tend to be): `abc…xyz`.
pub(crate) fn cut(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max / 2).collect();
    let tail: String = s.chars().skip(n - (max - 1 - max / 2)).collect();
    format!("{head}…{tail}")
}

/// The reply as one line: code fences, a `Summary:` label and line breaks removed.
/// Empty when the model said nothing.
pub fn parse(reply: &str) -> String {
    let words: Vec<&str> = reply
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .flat_map(str::split_whitespace)
        .collect();
    let s = words.join(" ");
    let label = s.get(..8).filter(|p| p.eq_ignore_ascii_case("summary:"));
    match label {
        Some(_) => s[8..].trim_start().to_string(),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(t: &str) -> AgentEvent {
        AgentEvent::Text { text: t.into() }
    }

    fn tool() -> AgentEvent {
        AgentEvent::ToolUse {
            name: "Bash".into(),
            input: json!({"command": "ls"}),
        }
    }

    #[test]
    fn wanted_after_tools_or_a_long_answer() {
        assert!(!wanted(&[text("4")], Some("4")));
        assert!(!wanted(&[], None));
        assert!(wanted(&[tool(), text("ok")], Some("ok")));
        let long = "x".repeat(SHORT_ANSWER_CHARS + 1);
        assert!(wanted(&[text(&long)], Some(&long)));
    }

    #[test]
    fn prompt_has_task_and_transcript_in_order() {
        let events = [
            AgentEvent::Started {
                session: None,
                model: None,
            },
            text("Looking."),
            tool(),
            AgentEvent::ToolResult {
                content: "a\nb\n".into(),
            },
            AgentEvent::Usage {
                input: 1,
                output: 1,
            },
            AgentEvent::Finished {
                ok: true,
                summary: Some("Two files.".into()),
            },
        ];
        let p = prompt("count files", &events);
        assert!(p.contains("1-2 plain sentences"));
        assert!(p.contains("Task:\ncount files\n"));
        assert!(p.ends_with("Transcript:\nLooking.\n> Bash {\"command\":\"ls\"}\n< a\nb\ndone"));
        assert!(!p.contains("omitted"));
    }

    #[test]
    fn prompt_keeps_latest_events_within_budget() {
        let big = "y".repeat(5_000);
        let mut events: Vec<AgentEvent> = (0..1000).map(|i| text(&format!("step {i}"))).collect();
        events.push(AgentEvent::ToolResult { content: big });
        events.push(text("last"));
        let p = prompt(&"p".repeat(10_000), &events);
        assert!(p.chars().count() < MAX_CHARS + 500, "{}", p.len());
        assert!(p.contains("(earlier events omitted)"));
        assert!(p.ends_with("yyy\nlast"));
        assert_eq!(cut("abcdefg", 5), "ab…fg");
        assert_eq!(cut("abcdefg", 4), "ab…g");
        assert!(p.contains("step 999") && !p.contains("step 0\n"));
    }

    #[test]
    fn parse_flattens_to_one_line() {
        assert_eq!(parse("  Did X.\nAll good.\n"), "Did X. All good.");
        assert_eq!(parse("```\nSummary: Did X.\n```"), "Did X.");
        assert_eq!(parse("summary:Did X."), "Did X.");
        assert_eq!(parse(" \n"), "");
        // Multi-byte text near the label check doesn't panic.
        assert_eq!(parse("é✓ ok"), "é✓ ok");
    }
}
