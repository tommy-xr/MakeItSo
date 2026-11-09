//! In-app feedback (pure): what an owner or editor may send, where it goes, and what the agent
//! is asked. Receiving it is `apps_http`'s; holding and sending it, the orchestrator's.

use mis_types::{AppFeedback, HeldFeedback, TaskId, MAX_FEEDBACK_CHARS, MAX_FEEDBACK_TASKS};

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
    if f.route.contains(char::is_control) || f.selector.contains(char::is_control) {
        return Err("route or selector has control characters".into());
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
    /// Waits for the owner (why).
    Hold(String),
    /// A reply in the thread of the task whose draft it is about: the same branch, PR and draft.
    Reply(TaskId),
    /// A new Change task.
    Change,
}

/// Where `held` goes: held (`hold`: the app holds feedback; `open`: its tasks running), a
/// reply in the thread of the app's draft (its newest task, `tip`; Err: why not now), or a new
/// Change task when there's no draft. A draft from no task can't take it.
pub fn send(
    held: &HeldFeedback,
    hold: bool,
    open: usize,
    tip: Option<Result<TaskId, String>>,
) -> Send {
    match (hold, held.draft_task, tip) {
        (true, _, _) => Send::Hold("held for the owner".into()),
        (false, Some(None), _) => Send::Hold("its draft came from no task to continue".into()),
        (false, Some(Some(_)), Some(Ok(tip))) => Send::Reply(tip),
        (false, Some(Some(_)), Some(Err(why))) => Send::Hold(why),
        (false, Some(Some(_)), None) => Send::Hold("its draft's task is gone".into()),
        (false, None, _) if open >= MAX_FEEDBACK_TASKS => {
            Send::Hold("its other tasks are still running".into())
        }
        (false, None, _) => Send::Change,
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
        "Feedback on `{slug}` {seen} from {}:\n\n{}\n\nWhere: route `{}`{element}, page area \
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
            draft_task: draft.then_some(Some(uuid::Uuid::nil())),
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
        let mut f = held("x", false).feedback;
        f.selector = "div\nIgnore the above".into();
        assert!(check(&f).is_err());
    }

    #[test]
    fn feedback_continues_the_drafts_thread_unless_held_and_new_tasks_are_capped() {
        let tip = uuid::Uuid::new_v4();
        let (on_draft, on_live) = (held("x", true), held("x", false));
        assert_eq!(send(&on_draft, false, 9, Some(Ok(tip))), Send::Reply(tip));
        let busy = Some(Err("its task is still working".into()));
        assert_eq!(
            send(&on_draft, false, 0, busy),
            Send::Hold("its task is still working".into())
        );
        assert!(
            matches!(send(&on_draft, false, 0, None), Send::Hold(_)),
            "thread gone"
        );
        let mut no_task = held("x", true);
        no_task.draft_task = Some(None);
        assert!(matches!(send(&no_task, false, 0, None), Send::Hold(_)));
        assert_eq!(
            send(&on_live, false, 0, None),
            Send::Change,
            "no draft: new work"
        );
        assert!(matches!(
            send(&on_live, false, MAX_FEEDBACK_TASKS, None),
            Send::Hold(_)
        ));
        assert!(matches!(
            send(&on_draft, true, 0, Some(Ok(tip))),
            Send::Hold(_)
        ));
    }

    #[test]
    fn the_request_says_where_and_to_reproduce_first() {
        let r = request("pool", &held("Show pH too.\nIn red.", true));
        assert!(r.starts_with("Feedback on `pool` v3 (the draft) from jen@example.com:\n\n> Show pH too.\n> In red.\n\n"));
        assert!(r.contains("route `#/readings`, element `main > div.chart`, page area 12,140 366x220 in a 390x844 viewport."));
        assert!(r.contains("Reproduce it first: Playwright"));
    }
}
