//! Planner: pure prompt construction and reply parsing. The shell runs the
//! model; nothing here does I/O.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Plan {
    pub harness: String,
    /// Rewritten prompt; empty keeps the original (see `parse`).
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub reason: String,
}

/// Prompt asking the model to pick a harness and strip dispatch instructions.
pub fn prompt(request: &str, harnesses: &[String]) -> String {
    let default = harnesses
        .iter()
        .find(|h| *h == "claude")
        .or(harnesses.first())
        .map(String::as_str)
        .unwrap_or("claude");
    format!(
        "You dispatch requests to a fleet of coding agents. Available harnesses: {}.\n\
         Pick the harness to run this request. Honor an explicit choice in the request \
         (\"use codex\", \"run as claude\"); otherwise pick {default}.\n\
         If the request contains dispatch instructions, set \"prompt\" to the request with \
         only those words removed, everything else verbatim; if nothing is left, ask the \
         agent to introduce itself. Otherwise omit \"prompt\" and the request is used as-is.\n\
         Reply with only a JSON object: {{\"harness\": \"...\", \"prompt\": \"...\", \"reason\": \"one short sentence\"}}\n\n\
         Request:\n{request}",
        harnesses.join(", ")
    )
}

/// Extract the plan from the model's reply (which may wrap the JSON in prose or
/// a code fence), check the harness exists, and fill an omitted prompt with `request`.
pub fn parse(reply: &str, request: &str, harnesses: &[String]) -> Result<Plan, String> {
    let start = reply.find('{').ok_or("planner reply has no JSON")?;
    let end = reply
        .rfind('}')
        .filter(|e| *e > start)
        .ok_or("planner reply has no JSON")?;
    let mut plan: Plan =
        serde_json::from_str(&reply[start..=end]).map_err(|e| format!("bad planner reply: {e}"))?;
    if !harnesses.contains(&plan.harness) {
        return Err(format!(
            "planner picked unknown harness {:?} (have {})",
            plan.harness,
            harnesses.join(", ")
        ));
    }
    if plan.prompt.trim().is_empty() {
        plan.prompt = request.to_string();
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> Vec<String> {
        vec!["claude".into(), "codex".into()]
    }

    #[test]
    fn parses_fenced_json_and_validates_harness() {
        let reply = "Sure.\n```json\n{\"harness\":\"codex\",\"prompt\":\"what agent are you?\",\"reason\":\"asked for codex\"}\n```";
        let p = parse(reply, "req", &both()).unwrap();
        assert_eq!(p.harness, "codex");
        assert_eq!(p.prompt, "what agent are you?");
        assert!(parse(reply, "req", &["claude".to_string()])
            .unwrap_err()
            .contains("unknown harness"));
        assert!(parse("no json here", "req", &both()).is_err());
        assert!(parse("} {", "req", &both()).is_err());
        // Omitted or blank prompt keeps the original request verbatim.
        let p = parse("{\"harness\":\"claude\",\"prompt\":\" \"}", "req", &both()).unwrap();
        assert_eq!(p.prompt, "req");
    }

    #[test]
    fn prompt_lists_harnesses_and_request() {
        let p = prompt("hey run as codex", &both());
        assert!(p.contains("claude, codex"));
        assert!(p.contains("otherwise pick claude"));
        assert!(p.ends_with("Request:\nhey run as codex"));
        assert!(prompt("x", &["codex".to_string()]).contains("otherwise pick codex"));
    }
}
