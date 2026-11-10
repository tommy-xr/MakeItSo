//! Reviews as ledger transitions (pure): approve means "merge when it can". Each look at the
//! PR (`review_assessed`) moves it on: merged, wait and look again, send a failure back to the
//! task that made it (at most [`MAX_FIX_ATTEMPTS`] times), or hand it to the owner.

use std::time::Duration;

use mis_types::*;
use uuid::Uuid;

use super::{Effect, Ledger};

/// How long to wait before looking at an approved PR again.
const RECHECK: Duration = Duration::from_secs(60);

impl Ledger {
    /// Every review, oldest first.
    pub fn reviews(&self) -> &[Review] {
        &self.reviews
    }

    pub fn review(&self, id: Uuid) -> Option<&Review> {
        self.reviews.iter().find(|r| r.id == id)
    }

    /// Add `review`. An app's newer draft replaces its undecided one.
    pub fn open_review(&mut self, review: Review) {
        if let ReviewKind::AppDraft { slug, .. } = &review.kind {
            let same =
                |r: &Review| matches!(&r.kind, ReviewKind::AppDraft { slug: s, .. } if s == slug);
            self.reviews.retain(|r| r.state.finished() || !same(r));
        }
        self.reviews.push(review);
    }

    /// The owner approves review `id` at its head: merge when it can. Err: why not.
    pub fn approve_review(&mut self, id: Uuid) -> Result<Vec<Effect>, String> {
        let r = self.review_mut(id)?;
        r.state = match &r.state {
            ReviewState::Open => ReviewState::Approved { fixes: 0 },
            ReviewState::FixReady { fixes, .. } => ReviewState::Approved { fixes: *fixes },
            ReviewState::NeedsYou { .. } => ReviewState::Approved { fixes: 0 },
            ReviewState::Approved { .. } => return Err("already approved".into()),
            ReviewState::Merged | ReviewState::Rejected => return Err("already decided".into()),
        };
        r.status = Some("approved: merging when it can".into());
        Ok(vec![Effect::AdvanceReview {
            id,
            after: Duration::ZERO,
        }])
    }

    /// The owner rejects review `id`: its PR is closed (and an app draft discarded).
    pub fn reject_review(&mut self, id: Uuid) -> Result<Vec<Effect>, String> {
        let r = self.review_mut(id)?;
        if r.state.finished() {
            return Err("already decided".into());
        }
        (r.state, r.status) = (ReviewState::Rejected, Some("rejected".into()));
        Ok(vec![Effect::ReviewDone { review: r.clone() }])
    }

    /// What a worker found looking at approved review `id`'s PR at its head.
    pub fn review_assessed(&mut self, id: Uuid, found: Assessment) -> Vec<Effect> {
        let Some(i) = self.reviews.iter().position(|r| r.id == id) else {
            return vec![];
        };
        let ReviewState::Approved { fixes } = self.reviews[i].state else {
            return vec![]; // decided, or a fix waits for the owner, meanwhile
        };
        let again = vec![Effect::AdvanceReview { id, after: RECHECK }];
        let r = &mut self.reviews[i];
        let (asked, why) = match found {
            Assessment::Merged => {
                (r.state, r.status) = (ReviewState::Merged, Some("merged".into()));
                return vec![Effect::ReviewDone { review: r.clone() }];
            }
            Assessment::Waiting { why } => {
                r.status = Some(why);
                return again;
            }
            Assessment::Moved { head } => {
                let approved = std::mem::replace(&mut r.head, head);
                r.state = ReviewState::FixReady { approved, fixes };
                r.status = Some("a fix is ready to look at".into());
                return vec![];
            }
            Assessment::Refused { why } => {
                (r.state, r.status) = (ReviewState::NeedsYou { why: why.clone() }, Some(why));
                return vec![];
            }
            Assessment::Failing { checks, log } => (
                fix_request(
                    &r.pr,
                    &format!("checks failed: {}", checks.join(", ")),
                    &log,
                ),
                format!("checks failed: {}", checks.join(", ")),
            ),
            Assessment::Conflict => (
                fix_request(&r.pr, "it conflicts with its base branch", ""),
                "it conflicts with its base branch".to_string(),
            ),
        };
        // Once per head: the same failure waits for the task's fix rather than asking again.
        if r.fixing.as_deref() == Some(r.head.as_str()) {
            r.status = Some(format!("{why}; waiting for its task's fix"));
            return again;
        }
        let tip = r.task.and_then(|t| self.thread_tip(t));
        let r = &mut self.reviews[i];
        match tip {
            _ if fixes >= MAX_FIX_ATTEMPTS => {
                let why = format!("{why}, after {fixes} fixes");
                (r.state, r.status) = (ReviewState::NeedsYou { why: why.clone() }, Some(why));
                vec![]
            }
            None => {
                (r.state, r.status) = (ReviewState::NeedsYou { why: why.clone() }, Some(why));
                vec![]
            }
            Some(Err(busy)) => {
                r.status = Some(format!("{why}; {busy}"));
                again
            }
            Some(Ok(tip)) => {
                let head = r.head.clone();
                let planner = false;
                match self.reply(tip, asked, vec![], planner) {
                    Ok((_, mut effects)) => {
                        let r = &mut self.reviews[i];
                        r.state = ReviewState::Approved { fixes: fixes + 1 };
                        r.fixing = Some(head);
                        let n = fixes + 1;
                        r.status = Some(format!(
                            "{why}; asked its task to fix it ({n}/{MAX_FIX_ATTEMPTS})"
                        ));
                        effects.extend(again);
                        effects
                    }
                    Err(e) => {
                        let r = &mut self.reviews[i];
                        let why = format!("{why}; couldn't ask its task: {e}");
                        (r.state, r.status) =
                            (ReviewState::NeedsYou { why: why.clone() }, Some(why));
                        vec![]
                    }
                }
            }
        }
    }

    fn review_mut(&mut self, id: Uuid) -> Result<&mut Review, String> {
        (self.reviews.iter_mut().find(|r| r.id == id)).ok_or_else(|| "no such review".into())
    }
}

/// What the task is asked when its approved PR can't merge.
fn fix_request(pr: &PrRef, problem: &str, log: &str) -> String {
    let log = match log.trim() {
        "" => String::new(),
        l => format!("\n\nThe failing check's log ends:\n\n```\n{l}\n```"),
    };
    format!(
        "Your PR {} was approved, but {problem}, so it can't merge.{log}\n\nFix it on the same \
         branch and push; nothing else changes. It merges once it passes, after the owner looks \
         at the fix.",
        pr.url
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(task: Option<TaskId>) -> Review {
        Review {
            id: Uuid::new_v4(),
            pr: PrRef {
                repo: "o/r".into(),
                number: 7,
                url: "https://github.com/o/r/pull/7".into(),
            },
            head: "a".repeat(40),
            task,
            note: None,
            kind: ReviewKind::Code,
            state: ReviewState::Open,
            at: 0,
            status: None,
            fixing: None,
            cwd: "~/r".into(),
        }
    }

    /// A ledger with one finished task (its thread's tip) on an online worker, and `r` for it.
    fn with_task() -> (Ledger, Uuid, TaskId) {
        let mut l = Ledger::default();
        let wid = Uuid::new_v4();
        l.connect(
            wid,
            super::super::tests::info(),
            &[],
            1,
            super::super::tests::T0,
        );
        let (task, _) = l.submit("p".into(), None, Some("claude".into()), None, vec![]);
        let t = l.tasks.iter_mut().find(|t| t.task.id == task).unwrap();
        (t.status, t.session, t.worker) = (TaskStatus::Done, Some("s".into()), Some(wid));
        let r = review(Some(task));
        let id = r.id;
        l.open_review(r);
        (l, id, task)
    }

    fn state(l: &Ledger, id: Uuid) -> ReviewState {
        l.review(id).unwrap().state.clone()
    }

    fn finish(l: &mut Ledger, task: TaskId) {
        for t in l.tasks.iter_mut().filter(|t| !t.status.finished()) {
            t.status = TaskStatus::Done;
            let _ = task;
        }
    }

    #[test]
    fn approve_waits_for_checks_then_merges_and_hands_on() {
        let (mut l, id, _) = with_task();
        assert!(matches!(
            l.approve_review(id).unwrap()[..],
            [Effect::AdvanceReview { .. }]
        ));
        assert!(l.approve_review(id).is_err());
        let waiting = Assessment::Waiting {
            why: "checks running".into(),
        };
        assert!(matches!(
            l.review_assessed(id, waiting)[..],
            [Effect::AdvanceReview { after: RECHECK, .. }]
        ));
        assert_eq!(
            l.review(id).unwrap().status.as_deref(),
            Some("checks running")
        );
        let done = l.review_assessed(id, Assessment::Merged);
        assert!(
            matches!(&done[..], [Effect::ReviewDone { review }] if review.state == ReviewState::Merged)
        );
        assert!(
            l.review_assessed(id, Assessment::Merged).is_empty(),
            "decided"
        );
    }

    #[test]
    fn failures_go_back_to_the_task_once_per_head_up_to_three_times() {
        let (mut l, id, task) = with_task();
        l.approve_review(id).unwrap();
        let failing = || Assessment::Failing {
            checks: vec!["verify".into()],
            log: "boom".into(),
        };
        let eff = l.review_assessed(id, failing());
        let reply = l.tasks.last().unwrap();
        assert_eq!(reply.parent, Some(task));
        assert!(
            reply.task.prompt.contains("checks failed: verify")
                && reply.task.prompt.contains("boom")
        );
        assert!(eff
            .iter()
            .any(|e| matches!(e, Effect::AdvanceReview { .. })));
        assert_eq!(state(&l, id), ReviewState::Approved { fixes: 1 });
        // Same head again: wait for the fix, ask nothing more.
        let n = l.tasks.len();
        l.review_assessed(id, failing());
        assert_eq!(l.tasks.len(), n);
        // A fix lands: it waits for the owner's nod at the new head.
        l.review_assessed(
            id,
            Assessment::Moved {
                head: "b".repeat(40),
            },
        );
        assert_eq!(
            state(&l, id),
            ReviewState::FixReady {
                approved: "a".repeat(40),
                fixes: 1
            }
        );
        assert_eq!(l.review(id).unwrap().head, "b".repeat(40));
        // Two more rounds, then the owner.
        for round in 2..=3 {
            finish(&mut l, task);
            l.approve_review(id).unwrap();
            l.review_assessed(id, Assessment::Conflict);
            assert_eq!(state(&l, id), ReviewState::Approved { fixes: round });
            l.review_assessed(
                id,
                Assessment::Moved {
                    head: format!("{round}").repeat(40),
                },
            );
        }
        finish(&mut l, task);
        l.approve_review(id).unwrap();
        l.review_assessed(id, failing());
        assert!(
            matches!(state(&l, id), ReviewState::NeedsYou { why } if why.contains("after 3 fixes"))
        );
    }

    #[test]
    fn no_task_or_a_closed_pr_needs_the_owner_and_reject_closes_it() {
        let mut l = Ledger::default();
        let r = review(None);
        let id = r.id;
        l.open_review(r);
        l.approve_review(id).unwrap();
        l.review_assessed(id, Assessment::Conflict);
        assert!(matches!(state(&l, id), ReviewState::NeedsYou { .. }));
        let (mut l, id, _) = with_task();
        l.approve_review(id).unwrap();
        l.review_assessed(
            id,
            Assessment::Refused {
                why: "it is closed".into(),
            },
        );
        assert_eq!(
            state(&l, id),
            ReviewState::NeedsYou {
                why: "it is closed".into()
            }
        );
        let eff = l.reject_review(id).unwrap();
        assert!(
            matches!(&eff[..], [Effect::ReviewDone { review }] if review.state == ReviewState::Rejected)
        );
        assert!(l.reject_review(id).is_err());
    }

    #[test]
    fn an_apps_newer_draft_replaces_its_undecided_review() {
        let mut l = Ledger::default();
        let draft = |release| Review {
            kind: ReviewKind::AppDraft {
                slug: "pool".into(),
                release,
            },
            ..review(None)
        };
        let (old, new) = (draft(2), draft(3));
        let (old_id, new_id) = (old.id, new.id);
        l.open_review(old);
        l.open_review(review(None));
        l.open_review(new);
        assert!(l.review(old_id).is_none() && l.review(new_id).is_some());
        assert_eq!(l.reviews().len(), 2);
    }
}
