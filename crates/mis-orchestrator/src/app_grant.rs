//! What an app task's token may do (pure): stage releases of the app its thread is bound
//! to, or, for a `+ App` thread, create exactly one new app. Never make one live.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

/// What a publish with the token does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Publish {
    /// A staged release of an app that exists.
    Existing,
    /// The thread's new app, with its first staged release, owned by `owner`.
    Create { owner: Option<String> },
}

/// Whether `b`'s token may publish `slug` (`activate`: live at once). Err: why not.
pub fn publish(b: &Binding, slug: &str, activate: bool) -> Result<Publish, String> {
    if activate {
        return Err("an app task stages releases; the owner makes them live".into());
    }
    match (&b.slug, b.create) {
        (Some(s), _) if s != slug => Err(format!("this task's app is {s}")),
        (_, true) => Ok(Publish::Create {
            owner: b.requester.clone(),
        }),
        (Some(_), false) => Ok(Publish::Existing),
        (None, false) => Err("this task has no app".into()),
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
        assert_eq!(publish(&b, "pool", false), Ok(Publish::Existing));
        assert!(publish(&b, "budget", false).unwrap_err().contains("pool"));
        assert!(publish(&b, "pool", true).unwrap_err().contains("owner"));
    }

    #[test]
    fn a_new_app_is_created_once_then_only_staged() {
        let owner = Some("me@example.com".to_string());
        // The agent picks the slug: any, until one exists.
        let b = binding(None, true);
        let create = Ok(Publish::Create {
            owner: owner.clone(),
        });
        assert_eq!(publish(&b, "chores", false), create);
        assert_eq!(publish(&b, "pool", false), create);
        assert!(publish(&b, "chores", true).is_err());
        // About to create one: only that slug, still creating.
        let r = reserve(b.clone(), "chores");
        assert_eq!(publish(&r, "chores", false), create);
        assert!(publish(&r, "pool", false).is_err());
        let b = created(b, "chores");
        assert_eq!(publish(&b, "chores", false), Ok(Publish::Existing));
        assert!(publish(&b, "pool", false).is_err());
        // The user named it: only that one.
        let b = binding(Some("chores"), true);
        assert_eq!(publish(&b, "chores", false), create);
        assert!(publish(&b, "pool", false).is_err());
        assert_eq!(
            publish(&created(b, "chores"), "chores", false),
            Ok(Publish::Existing)
        );
        assert!(publish(&binding(None, false), "x", false).is_err());
    }

    #[test]
    fn tokens_are_prefixed_and_unique() {
        let (a, b) = (mint(), mint());
        assert!(a.starts_with(TOKEN_PREFIX) && a.len() == TOKEN_PREFIX.len() + 64);
        assert_ne!(a, b);
    }
}
