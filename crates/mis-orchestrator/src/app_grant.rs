//! What an app task's token may do (pure): stage releases of the app its thread is bound
//! to, or, for a `+ App` thread, create exactly one new app. Never make one live.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::apps::Target;

/// The app an app task's thread works on; replies in the thread share it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// The thread's first task, so a create updates every task in it.
    pub thread: Uuid,
    /// The app it stages releases of. None: a new app whose slug the agent picks.
    pub slug: Option<String>,
    /// It may still create its app (`slug`, or any free slug if None): a `+ App` thread
    /// that hasn't yet.
    pub create: bool,
    /// Who asked (an email): owner of the app it creates. None: no login (the full token
    /// or loopback), so the app is shared with no one.
    pub requester: Option<String>,
}

/// Which apps `b`'s token may publish `slug` into (`activate`: live at once): the app it is
/// bound to, or the thread's new one, owned by its requester. Err: why not.
pub fn publish(b: &Binding, slug: &str, activate: bool) -> Result<Target, String> {
    if activate {
        return Err("an app task stages releases; the owner makes them live".into());
    }
    match (&b.slug, b.create) {
        (Some(s), _) if s != slug => Err(format!("this task's app is {s}")),
        (_, true) => Ok(Target::New {
            owner: b.requester.clone(),
            thread: b.thread,
        }),
        (Some(_), false) => Ok(Target::Existing),
        (None, false) => Err("this task has no app".into()),
    }
}

/// Whether a task bound as `b` is worth starting, `exists` saying if its named app has a
/// release: a new app can't take a name in use, a change needs its app. Err: why not.
pub fn startable(b: &Binding, exists: bool) -> Result<(), String> {
    match (&b.slug, b.create, exists) {
        (Some(s), true, true) => Err(format!("{s} exists already: ask for a change to it")),
        (Some(s), false, false) => Err(format!("there is no app {s} to change")),
        _ => Ok(()),
    }
}

/// `b` about to create `slug`: only that one, from now on.
pub fn reserve(b: Binding, slug: &str) -> Binding {
    Binding {
        slug: Some(slug.into()),
        ..b
    }
}

/// `b` once its app `slug` exists: bound to it, creating nothing more.
pub fn created(b: Binding, slug: &str) -> Binding {
    Binding {
        slug: Some(slug.into()),
        create: false,
        ..b
    }
}

/// A fresh app task token: 244 random bits, prefixed so a bearer is told apart cheaply.
pub fn mint() -> String {
    format!(
        "{TOKEN_PREFIX}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

pub const TOKEN_PREFIX: &str = "mis-app-";

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(slug: Option<&str>, create: bool) -> Binding {
        Binding {
            thread: Uuid::nil(),
            slug: slug.map(Into::into),
            create,
            requester: Some("me@example.com".into()),
        }
    }

    #[test]
    fn a_change_stages_only_its_own_app_and_never_activates() {
        let b = binding(Some("pool"), false);
        assert_eq!(publish(&b, "pool", false), Ok(Target::Existing));
        assert!(publish(&b, "budget", false).unwrap_err().contains("pool"));
        assert!(publish(&b, "pool", true).unwrap_err().contains("owner"));
    }

    #[test]
    fn a_new_app_is_created_once_then_only_staged() {
        let owner = Some("me@example.com".to_string());
        // The agent picks the slug: any, until one exists.
        let b = binding(None, true);
        let create = Ok(Target::New {
            owner: owner.clone(),
            thread: Uuid::nil(),
        });
        assert_eq!(publish(&b, "chores", false), create);
        assert_eq!(publish(&b, "pool", false), create);
        assert!(publish(&b, "chores", true).is_err());
        // About to create one: only that slug, still creating.
        let r = reserve(b.clone(), "chores");
        assert_eq!(publish(&r, "chores", false), create);
        assert!(publish(&r, "pool", false).is_err());
        let b = created(b, "chores");
        assert_eq!(publish(&b, "chores", false), Ok(Target::Existing));
        assert!(publish(&b, "pool", false).is_err());
        // The user named it: only that one.
        let b = binding(Some("chores"), true);
        assert_eq!(publish(&b, "chores", false), create);
        assert!(publish(&b, "pool", false).is_err());
        assert_eq!(
            publish(&created(b, "chores"), "chores", false),
            Ok(Target::Existing)
        );
        assert!(publish(&binding(None, false), "x", false).is_err());
    }

    #[test]
    fn a_new_app_needs_a_free_name_and_a_change_its_app() {
        assert!(startable(&binding(Some("pool"), true), true)
            .unwrap_err()
            .contains("change"));
        assert!(startable(&binding(Some("pool"), false), false).is_err());
        assert!(startable(&binding(Some("pool"), true), false).is_ok());
        assert!(startable(&binding(Some("pool"), false), true).is_ok());
        assert!(startable(&binding(None, true), true).is_ok());
    }

    #[test]
    fn tokens_are_prefixed_and_unique() {
        let (a, b) = (mint(), mint());
        assert!(a.starts_with(TOKEN_PREFIX) && a.len() == TOKEN_PREFIX.len() + 64);
        assert_ne!(a, b);
    }
}
