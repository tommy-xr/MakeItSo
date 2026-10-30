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
}

/// Whether `scope` may send `msg`. Exhaustive, so a new message must be classified.
pub fn allowed(scope: Scope, msg: &ClientMsg) -> bool {
    match msg {
        ClientMsg::Status | ClientMsg::Subscribe | ClientMsg::Show { .. } => true,
        // Seen clears attention for everyone; RefreshEvidence sets a worker running `gh`;
        // a push subscription makes the orchestrator POST task prompts to its endpoint;
        // a purpose rewrites a worker's config and steers routing; a suggestion runs a model.
        ClientMsg::Submit { .. }
        | ClientMsg::Seen { .. }
        | ClientMsg::RefreshEvidence { .. }
        | ClientMsg::PushSubscribe { .. }
        | ClientMsg::PushUnsubscribe { .. }
        | ClientMsg::SetPurpose { .. }
        | ClientMsg::SuggestPurpose { .. } => scope == Scope::Full,
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
        ];
        for m in acts {
            assert!(!allowed(Scope::Read, &m), "{m:?}");
            assert!(allowed(Scope::Full, &m), "{m:?}");
        }
    }
}
