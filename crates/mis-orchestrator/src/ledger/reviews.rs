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

    /// Add `review`. An app's newer draft replaces its undecided one; if that was approved and
    /// this is the same PR (its task's fix, proposed again), it is that fix, ready for the nod.
    pub fn open_review(&mut self, mut review: Review) {
        if let ReviewKind::AppDraft { slug, .. } = &review.kind {
            let same =
                |r: &Review| matches!(&r.kind, ReviewKind::AppDraft { slug: s, .. } if s == slug);
            let approved = (self.reviews.iter())
                .filter(|r| !r.state.finished() && same(r) && r.pr == review.pr)
                .find_map(|old| match old.state {
                    ReviewState::Approved { fixes } | ReviewState::FixReady { fixes, .. } => {
                        Some((old.head.clone(), fixes))
                    }
                    _ => None,
                });
            if let Some((approved, fixes)) = approved {
                review.state = ReviewState::FixReady { approved, fixes };
                review.status = Some("a fix is ready to look at".into());
            }
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
        r.status = Some("looking at its checks".into());
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
        if r.assessing {
            return Err("it may be merging right now: try again in a moment".into());
        }
        (r.state, r.status) = (ReviewState::Rejected, Some("rejected".into()));
        Ok(vec![Effect::ReviewDone { review: r.clone() }])
    }

    /// What a worker found looking at approved review `id`'s PR at its head.
    pub fn review_assessed(&mut self, id: Uuid, found: Assessment) -> Vec<Effect> {
        let Some(i) = self.reviews.iter().position(|r| r.id == id) else {
            return vec![];
        };
        self.reviews[i].assessing = false;
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
            // An app's new commits need their own release: it waits for its task to propose
            // them (which makes the fix ready) rather than merge code its draft doesn't hold.
            Assessment::Moved { .. } if matches!(r.kind, ReviewKind::AppDraft { .. }) => {
                r.status = Some("new commits: waiting for its task to propose them".into());
                return again;
            }
            Assessment::Moved { head } => {
                let approved = std::mem::replace(&mut r.head, head);
                r.state = ReviewState::FixReady { approved, fixes };
                r.status = Some("a fix is ready to look at".into());
                return vec![];
            }
            Assessment::Refused { why } => return needs_you(r, why),
            Assessment::Failing { checks, log } => {
                let why = format!("checks failed: {}", checks.join(", "));
                (fix_request(r, &why, &log), why)
            }
            Assessment::Conflict => {
                let why = "it conflicts with its base branch".to_string();
                (fix_request(r, &why, ""), why)
            }
        };
        let asked_here = r.fixing.as_deref() == Some(r.head.as_str());
        let tip = r.task.and_then(|t| self.thread_tip(t));
        let r = &mut self.reviews[i];
        match tip {
            // Asked once at this head: wait for the fix while its task works.
            Some(Err(busy)) if asked_here => {
                r.status = Some(format!("{why}; {busy}"));
                again
            }
            _ if asked_here => needs_you(r, format!("{why}; its task finished without a fix")),
            _ if fixes >= MAX_FIX_ATTEMPTS => needs_you(r, format!("{why}, after {fixes} fixes")),
            None => needs_you(r, why),
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
                    Err(e) => needs_you(
                        &mut self.reviews[i],
                        format!("{why}; couldn't ask its task: {e}"),
                    ),
                }
            }
        }
    }

    /// A worker is looking at approved review `id`'s PR (and may merge it): no rejecting
    /// it meanwhile. False: it isn't approved any more.
    pub fn assessing(&mut self, id: Uuid) -> bool {
        let r = self.reviews.iter_mut().find(|r| r.id == id);
        let Some(r) = r.filter(|r| matches!(r.state, ReviewState::Approved { .. })) else {
            return false;
        };
        r.assessing = true;
        true
    }

    /// Merged review `id`'s app release didn't go live (`why`): publishing again retries it.
    pub fn review_not_live(&mut self, id: Uuid, why: String) {
        if let Some(r) = self.reviews.iter_mut().find(|r| r.id == id) {
            needs_you(r, why);
        }
    }

    /// What became of review `id` after it was decided, e.g. its PR couldn't be closed.
    pub fn review_status(&mut self, id: Uuid, status: String) {
        if let Some(r) = self.reviews.iter_mut().find(|r| r.id == id) {
            r.status = Some(status);
        }
    }

    /// Every approved review: looked at again (after a restart).
    pub fn resume_reviews(&self) -> Vec<Effect> {
        (self.reviews.iter())
            .filter(|r| matches!(r.state, ReviewState::Approved { .. }))
            .map(|r| Effect::AdvanceReview {
                id: r.id,
                after: Duration::ZERO,
            })
            .collect()
    }

    fn review_mut(&mut self, id: Uuid) -> Result<&mut Review, String> {
        (self.reviews.iter_mut().find(|r| r.id == id)).ok_or_else(|| "no such review".into())
    }
}

/// `r` needs the owner: `why`. Nothing more to do on it until they act.
fn needs_you(r: &mut Review, why: String) -> Vec<Effect> {
    (r.state, r.status) = (ReviewState::NeedsYou { why: why.clone() }, Some(why));
    vec![]
}

/// What the task is asked when its approved PR can't merge.
fn fix_request(r: &Review, problem: &str, log: &str) -> String {
    let log = match log.trim() {
        "" => String::new(),
        l => format!("\n\nThe failing check's log ends:\n\n```\n{l}\n```"),
    };
    // An app's fix needs a release built from it, proposed as its draft again.
    let then = match r.kind {
        ReviewKind::AppDraft { .. } => "push, publish the new release and propose it again",
        ReviewKind::Code => "push",
    };
    format!(
        "Your PR {} was approved, but {problem}, so it can't merge.{log}\n\nFix it on the same \
         branch, then {then}; nothing else changes. It merges once it passes, after the owner \
         looks at the fix.",
        r.pr.url
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
            assessing: false,
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

    fn finish(l: &mut Ledger) {
        for t in l.tasks.iter_mut().filter(|t| !t.status.finished()) {
            t.status = TaskStatus::Done;
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
            finish(&mut l);
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
        finish(&mut l);
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
        // Approved, then proposed again on the same PR (its task's fix): the fix waits for a nod.
        l.approve_review(new_id).unwrap();
        let fix = Review {
            head: "c".repeat(40),
            ..draft(4)
        };
        let fix_id = fix.id;
        l.open_review(fix);
        let approved = "a".repeat(40);
        assert_eq!(
            state(&l, fix_id),
            ReviewState::FixReady { approved, fixes: 0 }
        );
    }
}
