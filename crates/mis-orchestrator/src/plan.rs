//! Planner: pure prompt construction and reply parsing. The shell runs the
//! model; nothing here does I/O.

use std::collections::BTreeMap;

use mis_types::{WorkerId, WorkerInfo};
use serde::Deserialize;

/// Harness -> models the planner may pick for it (empty: harness default only).
pub type Fleet = BTreeMap<String, Vec<String>>;

/// A known worker as the planner sees it, to route on its purpose and tools.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub id: WorkerId,
    /// Hostname, or `hostname-<id prefix>` when two workers share one.
    pub name: String,
    pub online: bool,
    pub harnesses: Vec<String>,
    pub purpose: Option<String>,
    /// `Inventory::brief`.
    pub tools: String,
}

/// Cards for `workers` (id, info, online), sorted by name.
pub fn cards<'a>(workers: impl IntoIterator<Item = (WorkerId, &'a WorkerInfo, bool)>) -> Vec<Card> {
    let all: Vec<_> = workers.into_iter().collect();
    let mut cards: Vec<Card> = all
        .iter()
        .map(|(id, info, online)| {
            let shared = all
                .iter()
                .filter(|(_, i, _)| i.hostname == info.hostname)
                .count()
                > 1;
            Card {
                id: *id,
                name: match shared {
                    true => format!("{}-{}", info.hostname, &id.to_string()[..8]),
                    false => info.hostname.clone(),
                },
                online: *online,
                harnesses: info.harnesses.clone(),
                purpose: info.purpose.clone(),
                tools: info.inventory.brief(),
            }
        })
        .collect();
    cards.sort_by(|a, b| a.name.cmp(&b.name));
    cards
}

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
    /// Card name of the only worker that should run it; see `pin`.
    #[serde(default)]
    pub worker: Option<String>,
    /// `worker` resolved by `parse`: the task waits for this worker, even offline.
    #[serde(skip)]
    pub pin: Option<WorkerId>,
}

/// JSON Schema the planner's reply must satisfy (`claude --json-schema`). The
/// harness and model are validated against the live fleet in `parse`, not here.
/// `parse` requires `harness` or `answer`; a harness wins over an answer.
pub const SCHEMA: &str = r#"{"type":"object","properties":{"harness":{"type":"string"},"prompt":{"type":"string"},"model":{"type":"string"},"reason":{"type":"string"},"answer":{"type":"string"},"worker":{"type":"string"}},"required":["reason"]}"#;

/// Prompt asking the model to answer a trivial question itself, or else pick a
/// harness and model and strip dispatch instructions. A request with `images`
/// attached always dispatches: only the agent gets to see them.
pub fn prompt(request: &str, images: usize, fleet: &Fleet, cards: &[Card]) -> String {
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
    let workers = workers_section(cards);
    format!(
        "You dispatch requests to a fleet of coding agents. Available harnesses: {}.\n\
         If the request names no harness, agent or model and is a trivial question you can \
         answer correctly with confidence, needing no tools, files, or access to any machine \
         (\"what is 2+2?\"), set \"answer\" to the answer and \"reason\", and nothing else.\n\
         Otherwise omit \"answer\" and pick the harness to run this request. Honor an \
         explicit choice in the request \
         (\"use codex\", \"run as claude\"); otherwise pick {default}.\n\
         {models}\
         {workers}\
         If the request contains dispatch instructions, set \"prompt\" to the request with \
         only those words removed, everything else verbatim; if nothing is left, ask the \
         agent to introduce itself. Otherwise omit \"prompt\" and the request is used as-is.\n\
         Set \"reason\" to one short sentence.\n\
         {attached}\n\
         Request:\n{request}",
        harnesses.join(", ")
    )
}

/// The workers and what each is for, when any worker says; empty otherwise.
fn workers_section(cards: &[Card]) -> String {
    if cards
        .iter()
        .all(|c| c.purpose.is_none() && c.tools.is_empty())
    {
        return String::new();
    }
    let lines: String = cards
        .iter()
        .map(|c| {
            let mut parts = vec![format!("harnesses {}", c.harnesses.join(", "))];
            parts.extend(c.purpose.as_ref().map(|p| format!("purpose: {p}")));
            if !c.tools.is_empty() {
                parts.push(c.tools.clone());
            }
            let off = if c.online { "" } else { " (offline)" };
            format!("- {}{off}: {}\n", c.name, parts.join("; "))
        })
        .collect();
    format!(
        "Workers:\n{lines}\
         If the request names a worker, or needs what only some workers have (their purpose, \
         apps or MCP servers; e.g. Blender work and a worker with Blender), set \"worker\" to \
         the best one's name and pick a harness it runs; the task waits for that worker if it \
         is offline or busy. Otherwise omit \"worker\" so any worker can run it.\n"
    )
}

/// Extract the plan from the model's reply (which may wrap the JSON in prose or
/// a code fence). A harness dispatches, even beside an answer (dispatching is the
/// safe side); then check it exists and fill an omitted prompt with `request`.
/// Without one, the answer stands alone. A named worker must exist and run the
/// harness; it becomes `pin`. A model not listed for the harness (blank, invented, or gone since
/// the prompt was built) falls back to the harness default.
pub fn parse(reply: &str, request: &str, fleet: &Fleet, cards: &[Card]) -> Result<Plan, String> {
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
    plan.worker = plan.worker.filter(|w| !w.trim().is_empty());
    if let Some(name) = &plan.worker {
        let card = cards
            .iter()
            .find(|c| &c.name == name)
            .ok_or_else(|| format!("planner picked unknown worker {name:?}"))?;
        if !card.harnesses.contains(&plan.harness) {
            return Err(format!(
                "planner picked {name}, which doesn't run {}",
                plan.harness
            ));
        }
        plan.pin = Some(card.id);
    }
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
        let p = parse(reply, "req", &both(), &[]).unwrap();
        assert_eq!(p.harness, "codex");
        assert_eq!(p.prompt, "what agent are you?");
        assert_eq!(p.model, None);
        let claude_only: Fleet = [("claude".into(), vec![])].into();
        assert!(parse(reply, "req", &claude_only, &[])
            .unwrap_err()
            .contains("unknown harness"));
        assert!(parse("no json here", "req", &both(), &[]).is_err());
        assert!(parse("} {", "req", &both(), &[]).is_err());
        // Omitted or blank prompt keeps the original request verbatim.
        let p = parse(
            "{\"harness\":\"claude\",\"prompt\":\" \"}",
            "req",
            &both(),
            &[],
        )
        .unwrap();
        assert_eq!(p.prompt, "req");
    }

    #[test]
    fn unlisted_model_falls_back_to_default() {
        let p = |reply| parse(reply, "req", &both(), &[]);
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
        let p = |reply| parse(reply, "what is 2+2?", &both(), &[]);
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
        let p = prompt("hey run as codex", 0, &both(), &[]);
        assert!(p.contains("claude, codex"));
        assert!(p.contains("otherwise pick claude"));
        assert!(p.contains("- claude: haiku, sonnet\n"));
        assert!(!p.contains("- codex"));
        assert!(p.contains("cheapest"));
        assert!(p.contains("names no harness, agent or model"));
        assert!(p.ends_with("Request:\nhey run as codex"));
        let codex: Fleet = [("codex".into(), vec![])].into();
        let p = prompt("x", 0, &codex, &[]);
        assert!(p.contains("otherwise pick codex"));
        assert!(!p.contains("\"model\""));
        assert!(!p.contains("attached"));
        let p = prompt("what is this?", 2, &codex, &[]);
        assert!(p.contains("attached 2 image(s) that only the agent will see"));
        assert!(p.ends_with("yourself.\n\nRequest:\nwhat is this?"));
    }

    fn blender_fleet() -> Vec<Card> {
        let blender = WorkerInfo {
            hostname: "studio".into(),
            harnesses: vec!["claude".into()],
            purpose: Some("Blender + blender-mcp; 3D modelling".into()),
            inventory: mis_types::Inventory {
                mcp: [("claude".into(), vec!["blender".into()])].into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let laptop = WorkerInfo {
            hostname: "laptop".into(),
            harnesses: vec!["claude".into(), "codex".into()],
            ..Default::default()
        };
        let a = WorkerId::from_u128(1);
        let b = WorkerId::from_u128(2);
        cards([(a, &blender, false), (b, &laptop, true)])
    }

    #[test]
    fn prompt_lists_workers_only_when_one_says_what_it_is_for() {
        let p = prompt("model a chair", 0, &both(), &blender_fleet());
        assert!(p.contains(
            "- studio (offline): harnesses claude; purpose: Blender + blender-mcp; 3D modelling; \
             mcp claude: blender\n"
        ));
        assert!(p.contains("- laptop: harnesses claude, codex\n"));
        assert!(p.contains("set \"worker\""));
        let plain = cards([(WorkerId::nil(), &WorkerInfo::default(), true)]);
        assert!(!prompt("x", 0, &both(), &plain).contains("Workers:"));
    }

    #[test]
    fn named_worker_must_exist_and_run_the_harness() {
        let fleet = blender_fleet();
        let p = |reply| parse(reply, "model a chair", &both(), &fleet);
        let ok = p(r#"{"harness":"claude","worker":"studio"}"#).unwrap();
        assert_eq!(ok.pin, Some(WorkerId::from_u128(1)));
        for reply in [
            r#"{"harness":"claude"}"#,
            r#"{"harness":"claude","worker":" "}"#,
        ] {
            assert_eq!(p(reply).unwrap().pin, None, "{reply}");
        }
        let e = p(r#"{"harness":"claude","worker":"mars"}"#).unwrap_err();
        assert!(e.contains("unknown worker"), "{e}");
        let e = p(r#"{"harness":"codex","worker":"studio"}"#).unwrap_err();
        assert!(e.contains("doesn't run codex"), "{e}");
    }

    #[test]
    fn shared_hostnames_get_id_suffixes() {
        let info = WorkerInfo {
            hostname: "mac".into(),
            ..Default::default()
        };
        let id = |s: &str| s.parse::<WorkerId>().unwrap();
        let (a, b) = (
            id("bbbbbbbb-0000-4000-8000-000000000000"),
            id("aaaaaaaa-0000-4000-8000-000000000000"),
        );
        let names: Vec<String> = cards([(a, &info, true), (b, &info, false)])
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["mac-aaaaaaaa", "mac-bbbbbbbb"]);
    }
}
