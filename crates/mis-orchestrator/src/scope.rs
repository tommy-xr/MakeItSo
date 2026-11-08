//! What a client's credentials let it do (pure).

use mis_types::ClientMsg;
use serde::Serialize;

/// Granted when a request is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The full token, a Cloudflare Access login, or no token configured.
    Full,
    /// A read-only token: may look, not act.
    Read,
    /// A capture token: may only `POST /api/capture`.
    Capture,
}

/// Where a request goes: capture credentials open only `POST /api/capture`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Capture,
    /// Every other HTTP route and WebSocket.
    Other,
}

/// Whether `scope` may use `route` at all. Exhaustive, so a new scope must be classified.
pub fn reaches(scope: Scope, route: Route) -> bool {
    match (scope, route) {
        (Scope::Full, _) | (Scope::Read, Route::Other) | (Scope::Capture, Route::Capture) => true,
        // Capturing is acting, which read tokens may not; capture tokens may do nothing else.
        (Scope::Read, Route::Capture) | (Scope::Capture, Route::Other) => false,
    }
}

/// Whether `scope` may send `msg`. Exhaustive, so a new message and scope must be classified.
pub fn allowed(scope: Scope, msg: &ClientMsg) -> bool {
    let acts = match msg {
        ClientMsg::Status | ClientMsg::Subscribe | ClientMsg::Show { .. } => false,
        // Seen clears attention for everyone; RefreshEvidence sets a worker running `gh`;
        // a push subscription makes the orchestrator POST task prompts to its endpoint;
        // a purpose rewrites a worker's config and steers routing; a suggestion runs a model;
        // an approval lets an agent act (and "always" rewrites a worker's config);
        // sessions hold what the user's own terminals say, and a bell pushes what they say.
        ClientMsg::Submit { .. }
        | ClientMsg::Seen { .. }
        | ClientMsg::RefreshEvidence { .. }
        | ClientMsg::PushSubscribe { .. }
        | ClientMsg::PushUnsubscribe { .. }
        | ClientMsg::SetPurpose { .. }
        | ClientMsg::SuggestPurpose { .. }
        | ClientMsg::Approve { .. }
        | ClientMsg::WatchSessions
        | ClientMsg::SessionBell { .. } => true,
    };
    match scope {
        Scope::Full => true,
        Scope::Read => !acts,
        Scope::Capture => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_scope_may_look_not_act() {
        let submit = |reply_to| ClientMsg::Submit {
            prompt: "p".into(),
            cwd: None,
            harness: None,
            model: None,
            reply_to,
            images: vec![],
        };
        let looks = [
            ClientMsg::Status,
            ClientMsg::Subscribe,
            ClientMsg::Show {
                task: uuid::Uuid::nil(),
            },
        ];
        for m in &looks {
            assert!(allowed(Scope::Read, m), "{m:?}");
            assert!(allowed(Scope::Full, m), "{m:?}");
            assert!(!allowed(Scope::Capture, m), "{m:?}");
        }
        let task = uuid::Uuid::nil();
        let acts = [
            submit(None),
            submit(Some(task)),
            ClientMsg::Seen { task },
            ClientMsg::RefreshEvidence { task },
            ClientMsg::PushSubscribe {
                endpoint: "e".into(),
                p256dh: "k".into(),
                auth: "a".into(),
            },
            ClientMsg::PushUnsubscribe {
                endpoint: "e".into(),
            },
            ClientMsg::SetPurpose {
                worker: task,
                purpose: "p".into(),
            },
            ClientMsg::SuggestPurpose { worker: task },
            ClientMsg::Approve {
                task,
                id: task,
                decision: mis_types::Decision::Allow,
            },
            ClientMsg::WatchSessions,
            ClientMsg::SessionBell {
                worker: task,
                harness: "claude".into(),
                id: "s".into(),
                on: true,
            },
        ];
        for m in acts {
            assert!(!allowed(Scope::Read, &m), "{m:?}");
            assert!(allowed(Scope::Full, &m), "{m:?}");
            assert!(!allowed(Scope::Capture, &m), "{m:?}");
        }
    }

    #[test]
    fn capture_scope_reaches_only_the_capture_route() {
        use Route::*;
        assert!(reaches(Scope::Full, Capture) && reaches(Scope::Full, Other));
        assert!(!reaches(Scope::Read, Capture) && reaches(Scope::Read, Other));
        assert!(reaches(Scope::Capture, Capture) && !reaches(Scope::Capture, Other));
    }
}
