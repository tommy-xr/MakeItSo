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
        ClientMsg::Submit { .. } => scope == Scope::Full,
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
        for m in [submit(None), submit(Some(uuid::Uuid::nil()))] {
            assert!(!allowed(Scope::Read, &m), "{m:?}");
            assert!(allowed(Scope::Full, &m), "{m:?}");
        }
    }
}
