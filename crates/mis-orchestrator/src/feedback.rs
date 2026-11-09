//! In-app feedback (pure): what an owner or editor may send, where it goes, and what the agent
//! is asked. Receiving it is `apps_http`'s; holding and sending it, the orchestrator's.

use mis_types::{AppFeedback, HeldFeedback, TaskId, MAX_FEEDBACK_CHARS};

/// Err: why `f` isn't feedback to act on.
pub fn check(f: &AppFeedback) -> Result<(), String> {
    let note = f.note.trim();
    if note.is_empty() {
        return Err("say what to change".into());
    }
    if note.chars().count() > MAX_FEEDBACK_CHARS {
        return Err(format!("the note is over {MAX_FEEDBACK_CHARS} characters"));
    }
    if f.route.len() > 300 || f.selector.len() > 500 {
        return Err("route or selector too long".into());
    }
    let r = f.rect;
    let ok = |v: f64| v.is_finite() && v.abs() <= 100_000.0;
    if ![r.x, r.y, r.w, r.h].into_iter().all(ok) || r.w < 0.0 || r.h < 0.0 {
        return Err("rect isn't an area on the page".into());
    }
    let v = f.viewport;
    if !(1..=10_000).contains(&v.w) || !(1..=10_000).contains(&v.h) {
        return Err("viewport isn't a screen size".into());
    }
    Ok(())
}

/// Where feedback goes.
#[derive(Debug, PartialEq)]
pub enum Send {
    /// Waits for the owner (`why`).
    Hold(&'static str),
    /// A reply in the thread of the task whose draft it is about: the same branch, PR and draft.
    Reply(TaskId),
    /// A new Change task.
    Change,
}

/// Where `held` goes when it comes in: `hold` (the app holds feedback), and the task of the
/// app's draft (`draft_task`) if `held` is about that draft.
pub fn send(held: &HeldFeedback, hold: bool, draft_task: Option<TaskId>) -> Send {
    match (hold, held.draft, draft_task) {
        (true, _, _) => Send::Hold("held for the owner"),
        (false, true, Some(task)) => Send::Reply(task),
        _ => Send::Change,
    }
}

/// What the agent is asked: the note, where it was marked, and to reproduce it first.
pub fn request(slug: &str, held: &HeldFeedback) -> String {
    let f = &held.feedback;
    let seen = match (held.release, held.draft) {
        (Some(n), true) => format!("v{n} (the draft)"),
        (Some(n), false) => format!("v{n} (live)"),
        (None, _) => "it".into(),
    };
    let quoted: Vec<String> = f.note.trim().lines().map(|l| format!("> {l}")).collect();
    let (r, v) = (f.rect, f.viewport);
    let element = match f.selector.trim() {
        "" => String::new(),
        s => format!(", element `{s}`"),
    };
    format!(
        "Feedback on `{slug}` {seen} from {}:\n\n{}\n\nWhere: route `{}`{element}, area \
         {:.0},{:.0} {:.0}x{:.0} in a {}x{} viewport.\n\nReproduce it first: Playwright at that \
         route and viewport (mock mode), screenshot that element: the before shot. Then fix it, \
         `npm run verify`, take the after shot at the same route and viewport, publish and \
         propose: the new release replaces the draft.",
        held.by,
        quoted.join("\n"),
        f.route,
        r.x,
        r.y,
        r.w,
        r.h,
        v.w,
        v.h
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mis_types::{FeedbackRect, FeedbackSize};

    fn held(note: &str, draft: bool) -> HeldFeedback {
        HeldFeedback {
            id: uuid::Uuid::nil(),
            at: 0,
            by: "jen@example.com".into(),
            release: Some(3),
            draft,
            feedback: AppFeedback {
                route: "#/readings".into(),
                selector: "main > div.chart".into(),
                rect: FeedbackRect {
                    x: 12.4,
                    y: 140.0,
                    w: 366.0,
                    h: 220.0,
                },
                viewport: FeedbackSize { w: 390, h: 844 },
                note: note.into(),
            },
            waiting: None,
        }
    }

    #[test]
    fn feedback_needs_a_note_and_a_real_area() {
        assert_eq!(check(&held("Show pH too.", false).feedback), Ok(()));
        assert!(check(&held("  ", false).feedback).is_err());
        let long = "x".repeat(MAX_FEEDBACK_CHARS + 1);
        assert!(check(&held(&long, false).feedback).is_err());
        let mut f = held("x", false).feedback;
        f.rect.w = f64::NAN;
        assert!(check(&f).is_err());
        let mut f = held("x", false).feedback;
        f.viewport.w = 0;
        assert!(check(&f).is_err());
    }

    #[test]
    fn draft_feedback_continues_its_thread_unless_held() {
        let t = uuid::Uuid::new_v4();
        assert_eq!(send(&held("x", true), false, Some(t)), Send::Reply(t));
        assert_eq!(
            send(&held("x", false), false, Some(t)),
            Send::Change,
            "about live"
        );
        assert_eq!(
            send(&held("x", true), false, None),
            Send::Change,
            "no draft task"
        );
        assert!(matches!(
            send(&held("x", true), true, Some(t)),
            Send::Hold(_)
        ));
    }

    #[test]
    fn the_request_says_where_and_to_reproduce_first() {
        let r = request("pool", &held("Show pH too.\nIn red.", true));
        assert!(r.starts_with("Feedback on `pool` v3 (the draft) from jen@example.com:\n\n> Show pH too.\n> In red.\n\n"));
        assert!(r.contains("route `#/readings`, element `main > div.chart`, area 12,140 366x220 in a 390x844 viewport."));
        assert!(r.contains("Reproduce it first: Playwright"));
    }
}
