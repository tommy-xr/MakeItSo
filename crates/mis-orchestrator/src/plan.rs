//! Planner: pure prompt construction and reply parsing. The shell runs the
//! model; nothing here does I/O.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Plan {
    pub harness: String,
    pub prompt: String,
    #[serde(default)]
    pub reason: String,
}

/// Prompt asking the model to pick a harness and rewrite the request.
pub fn prompt(request: &str, harnesses: &[String]) -> String {
    format!(
        "You dispatch requests to a fleet of coding agents. Available harnesses: {}.\n\
         Pick the harness to run this request. Honor an explicit choice in the request \
         (\"use codex\", \"run as claude\"); otherwise pick claude.\n\
         Rewrite the request as the prompt the agent will receive: drop the dispatch \
         instructions, keep every detail of the actual work. If nothing is left, ask the \
         agent to introduce itself.\n\
         Reply with only a JSON object: {{\"harness\": \"...\", \"prompt\": \"...\", \"reason\": \"one short sentence\"}}\n\n\
         Request:\n{request}",
        harnesses.join(", ")
    )
}

/// Extract the plan from the model's reply (which may wrap the JSON in prose or
/// a code fence) and check the harness exists.
pub fn parse(reply: &str, harnesses: &[String]) -> Result<Plan, String> {
    let start = reply.find('{').ok_or("planner reply has no JSON")?;
    let end = reply.rfind('}').ok_or("planner reply has no JSON")?;
    let plan: Plan =
        serde_json::from_str(&reply[start..=end]).map_err(|e| format!("bad planner reply: {e}"))?;
    if !harnesses.contains(&plan.harness) {
        return Err(format!(
            "planner picked unknown harness {:?} (have {})",
            plan.harness,
            harnesses.join(", ")
        ));
    }
    if plan.prompt.trim().is_empty() {
        return Err("planner produced an empty prompt".into());
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
        let p = parse(reply, &both()).unwrap();
        assert_eq!(p.harness, "codex");
        assert_eq!(p.prompt, "what agent are you?");
        assert!(parse(reply, &["claude".to_string()])
            .unwrap_err()
            .contains("unknown harness"));
        assert!(parse("no json here", &both()).is_err());
        assert!(parse("{\"harness\":\"claude\",\"prompt\":\" \"}", &both()).is_err());
    }

    #[test]
    fn prompt_lists_harnesses_and_request() {
        let p = prompt("hey run as codex", &both());
        assert!(p.contains("claude, codex"));
        assert!(p.ends_with("Request:\nhey run as codex"));
    }
}
