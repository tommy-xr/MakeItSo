//! Planner: pure prompt construction and reply parsing. The shell runs the
//! model; nothing here does I/O.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Harness -> models the planner may pick for it (empty: harness default only).
pub type Fleet = BTreeMap<String, Vec<String>>;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Plan {
    /// Empty when `answer` is set.
    #[serde(default)]
    pub harness: String,
    /// Rewritten prompt; empty keeps the original (see `parse`).
    #[serde(default)]
    pub prompt: String,
    /// None runs the harness's default model.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reason: String,
    /// The planner answered a trivial question itself; nothing is dispatched.
    #[serde(default)]
    pub answer: Option<String>,
}

/// JSON Schema the planner's reply must satisfy (`claude --json-schema`). The
/// harness and model are validated against the live fleet in `parse`, not here.
/// `parse` requires `harness` or `answer`; a harness wins over an answer.
pub const SCHEMA: &str = r#"{"type":"object","properties":{"harness":{"type":"string"},"prompt":{"type":"string"},"model":{"type":"string"},"reason":{"type":"string"},"answer":{"type":"string"}},"required":["reason"]}"#;

/// Prompt asking the model to answer a trivial question itself, or else pick a
/// harness and model and strip dispatch instructions. A request with `images`
/// attached always dispatches: only the agent gets to see them.
pub fn prompt(request: &str, images: usize, fleet: &Fleet) -> String {
    let harnesses: Vec<&str> = fleet.keys().map(String::as_str).collect();
    let default = harnesses
        .iter()
        .find(|h| **h == "claude")
        .or(harnesses.first())
        .copied()
        .unwrap_or("claude");
    let models: String = fleet
        .iter()
        .filter(|(_, m)| !m.is_empty())
        .map(|(h, m)| format!("- {h}: {}\n", m.join(", ")))
        .collect();
    let models = if models.is_empty() {
        String::new()
    } else {
        format!(
            "Models per harness:\n{models}\
             If the chosen harness lists models, always set \"model\" to one of them: the one \
             the request names, otherwise the cheapest adequate for it. For claude: haiku for \
             trivial lookups or summaries, sonnet for ordinary coding, opus for hard multi-file \
             engineering, fable only when the request clearly demands the most capable model. \
             Other harnesses list models in preference order; use the first unless the request \
             calls for another. Otherwise omit \"model\".\n"
        )
    };
    let attached = match images {
        0 => String::new(),
        n => format!(
            "The user attached {n} image(s) that only the agent will see, so never \
             answer this request yourself.\n"
        ),
    };
    format!(
        "You dispatch requests to a fleet of coding agents. Available harnesses: {}.\n\
         If the request names no harness, agent or model and is a trivial question you can \
         answer correctly with confidence, needing no tools, files, or access to any machine \
         (\"what is 2+2?\"), set \"answer\" to the answer and \"reason\", and nothing else.\n\
         Otherwise omit \"answer\" and pick the harness to run this request. Honor an \
         explicit choice in the request \
         (\"use codex\", \"run as claude\"); otherwise pick {default}.\n\
         {models}\
         If the request contains dispatch instructions, set \"prompt\" to the request with \
         only those words removed, everything else verbatim; if nothing is left, ask the \
         agent to introduce itself. Otherwise omit \"prompt\" and the request is used as-is.\n\
         Set \"reason\" to one short sentence.\n\
         {attached}\n\
         Request:\n{request}",
        harnesses.join(", ")
    )
}

/// Extract the plan from the model's reply (which may wrap the JSON in prose or
/// a code fence). A harness dispatches, even beside an answer (dispatching is the
/// safe side); then check it exists and fill an omitted prompt with `request`.
/// Without one, the answer stands alone. A model not listed for the harness (blank, invented, or gone since
/// the prompt was built) falls back to the harness default.
pub fn parse(reply: &str, request: &str, fleet: &Fleet) -> Result<Plan, String> {
    let start = reply.find('{').ok_or("planner reply has no JSON")?;
    let end = reply
        .rfind('}')
        .filter(|e| *e > start)
        .ok_or("planner reply has no JSON")?;
    let mut plan: Plan =
        serde_json::from_str(&reply[start..=end]).map_err(|e| format!("bad planner reply: {e}"))?;
    plan.answer = plan.answer.filter(|a| !a.trim().is_empty());
    if plan.harness.is_empty() {
        return match plan.answer {
            Some(_) => Ok(plan),
            None => Err("planner set neither answer nor harness".into()),
        };
    }
    plan.answer = None;
    let Some(models) = fleet.get(&plan.harness) else {
        return Err(format!(
            "planner picked unknown harness {:?} (have {})",
            plan.harness,
            fleet.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    };
    plan.model = plan.model.filter(|m| models.contains(m));
    if plan.prompt.trim().is_empty() {
        plan.prompt = request.to_string();
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> Fleet {
        [
            ("claude".into(), vec!["haiku".into(), "sonnet".into()]),
            ("codex".into(), vec![]),
        ]
        .into()
    }

    #[test]
    fn parses_fenced_json_and_validates_harness() {
        let reply = "Sure.\n```json\n{\"harness\":\"codex\",\"prompt\":\"what agent are you?\",\"reason\":\"asked for codex\"}\n```";
        let p = parse(reply, "req", &both()).unwrap();
        assert_eq!(p.harness, "codex");
        assert_eq!(p.prompt, "what agent are you?");
        assert_eq!(p.model, None);
        let claude_only: Fleet = [("claude".into(), vec![])].into();
        assert!(parse(reply, "req", &claude_only)
            .unwrap_err()
            .contains("unknown harness"));
        assert!(parse("no json here", "req", &both()).is_err());
        assert!(parse("} {", "req", &both()).is_err());
        // Omitted or blank prompt keeps the original request verbatim.
        let p = parse("{\"harness\":\"claude\",\"prompt\":\" \"}", "req", &both()).unwrap();
        assert_eq!(p.prompt, "req");
    }

    #[test]
    fn unlisted_model_falls_back_to_default() {
        let p = |reply| parse(reply, "req", &both());
        let ok = p(r#"{"harness":"claude","model":"haiku"}"#).unwrap();
        assert_eq!(ok.model.as_deref(), Some("haiku"));
        // Blank, listed for another harness, or not at all: harness default.
        for reply in [
            r#"{"harness":"claude","model":" "}"#,
            r#"{"harness":"codex","model":"haiku"}"#,
            r#"{"harness":"claude","model":"opus"}"#,
        ] {
            assert_eq!(p(reply).unwrap().model, None, "{reply}");
        }
    }

    #[test]
    fn answer_xor_harness() {
        let p = |reply| parse(reply, "what is 2+2?", &both());
        let a = p(r#"{"answer":"4","reason":"arithmetic"}"#).unwrap();
        assert_eq!((a.answer.as_deref(), a.harness.as_str()), (Some("4"), ""));
        // Both: dispatch wins.
        let d = p(r#"{"answer":"4","harness":"claude"}"#).unwrap();
        assert_eq!((d.answer, d.harness.as_str()), (None, "claude"));
        // Blank answer counts as none.
        for reply in [r#"{"reason":"x"}"#, r#"{"answer":" "}"#] {
            assert!(p(reply).unwrap_err().contains("neither"), "{reply}");
        }
        assert_eq!(
            p(r#"{"answer":"","harness":"claude"}"#).unwrap().answer,
            None
        );
    }

    #[test]
    fn prompt_lists_harnesses_models_and_request() {
        let p = prompt("hey run as codex", 0, &both());
        assert!(p.contains("claude, codex"));
        assert!(p.contains("otherwise pick claude"));
        assert!(p.contains("- claude: haiku, sonnet\n"));
        assert!(!p.contains("- codex"));
        assert!(p.contains("cheapest"));
        assert!(p.contains("names no harness, agent or model"));
        assert!(p.ends_with("Request:\nhey run as codex"));
        let codex: Fleet = [("codex".into(), vec![])].into();
        let p = prompt("x", 0, &codex);
        assert!(p.contains("otherwise pick codex"));
        assert!(!p.contains("\"model\""));
        assert!(!p.contains("attached"));
        let p = prompt("what is this?", 2, &codex);
        assert!(p.contains("attached 2 image(s) that only the agent will see"));
        assert!(p.ends_with("yourself.\n\nRequest:\nwhat is this?"));
    }
}
