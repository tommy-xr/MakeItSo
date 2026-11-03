//! The agent's own checklist for its task (Claude's `TodoWrite`, Codex's todo
//! list): each update replaces the whole list.

use serde::{Deserialize, Serialize};

use crate::first_line;

/// Rows kept of one plan; the rest are dropped.
pub const MAX_PLAN_ITEMS: usize = 50;
/// Chars kept of a row's text (one line).
pub const MAX_PLAN_ITEM_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanStatus,
    /// What the agent says while on it, e.g. `Running the tests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

/// `items` cut to size: the first `MAX_PLAN_ITEMS` non-blank rows, each text
/// one line of at most `MAX_PLAN_ITEM_CHARS`.
pub fn bound_plan(items: Vec<PlanItem>) -> Vec<PlanItem> {
    let line = |s: &str| Some(first_line(s, MAX_PLAN_ITEM_CHARS)).filter(|l| !l.trim().is_empty());
    items
        .into_iter()
        .filter_map(|i| {
            Some(PlanItem {
                text: line(&i.text)?,
                status: i.status,
                active_form: i.active_form.as_deref().and_then(line),
            })
        })
        .take(MAX_PLAN_ITEMS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> PlanItem {
        PlanItem {
            text: text.into(),
            status: PlanStatus::Pending,
            active_form: Some(format!("{text}ing")),
        }
    }

    #[test]
    fn plans_are_bounded() {
        let mut items: Vec<_> = (0..60).map(|i| item(&format!("step {i}"))).collect();
        items.insert(1, item(" \n"));
        items[0].text = format!("{}\nsecond line", "x".repeat(300));
        items[0].active_form = Some(" ".into());
        let got = bound_plan(items);
        assert_eq!(got.len(), MAX_PLAN_ITEMS);
        assert_eq!(got[0].text.chars().count(), MAX_PLAN_ITEM_CHARS);
        assert!(got[0].text.ends_with('…'));
        assert_eq!(got[0].active_form, None, "blank active form dropped");
        assert_eq!(got[1].text, "step 1", "blank row dropped");
        assert_eq!(got[49].text, "step 49");
    }

    #[test]
    fn wire_shape() {
        let i = PlanItem {
            text: "Run tests".into(),
            status: PlanStatus::InProgress,
            active_form: None,
        };
        let j = serde_json::to_string(&i).unwrap();
        assert_eq!(j, r#"{"text":"Run tests","status":"in_progress"}"#);
        assert_eq!(serde_json::from_str::<PlanItem>(&j).unwrap(), i);
    }
}
