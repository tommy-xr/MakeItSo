//! Who may use an app (pure): members and their roles, and how the apps listener
//! identifies a request and checks its Host. `apps_http` is the shell around it.

use std::collections::BTreeMap;

use mis_types::{AppMember, AppRole};
use serde::{Deserialize, Serialize};

/// Who is asking on the apps listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// A direct loopback request without Cloudflare headers (dev, tests): owner of every app.
    Local,
    /// An Access login: its email (or a service token's client id), lower-case.
    User(String),
}

impl Subject {
    /// `local`, or the email.
    pub fn name(&self) -> &str {
        match self {
            Subject::Local => "local",
            Subject::User(e) => e,
        }
    }
}

/// A request's subject and role in the app its path names: in request extensions for
/// handlers under `/a/<slug>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub subject: Subject,
    pub role: AppRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    /// Change the app itself: its members, its data wholesale.
    Admin,
}

/// `<state>/apps/<slug>/policy.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// Email -> role.
    #[serde(default)]
    pub members: BTreeMap<String, AppRole>,
}

impl Policy {
    /// By email.
    pub fn list(&self) -> Vec<AppMember> {
        (self.members.iter())
            .map(|(email, role)| AppMember {
                email: email.clone(),
                role: *role,
            })
            .collect()
    }
}

/// `subject`'s role in an app with `policy`. Emails match ignoring case.
pub fn role_of(policy: &Policy, subject: &Subject) -> Option<AppRole> {
    match subject {
        Subject::Local => Some(AppRole::Owner),
        Subject::User(who) => (policy.members.iter())
            .find(|(email, _)| email.eq_ignore_ascii_case(who))
            .map(|(_, role)| *role),
    }
}

/// Exhaustive, so a new role or action must be classified.
pub fn allows(role: AppRole, action: Action) -> bool {
    match (role, action) {
        (AppRole::Owner, _) => true,
        (AppRole::Editor, Action::Read | Action::Write) => true,
        (AppRole::Editor, Action::Admin) => false,
        (AppRole::Viewer, Action::Read) => true,
        (AppRole::Viewer, Action::Write | Action::Admin) => false,
    }
}

/// `policy` with `email` given `role`, or removed for None. Err: not an email.
pub fn set_member(
    mut policy: Policy,
    email: &str,
    role: Option<AppRole>,
) -> Result<Policy, String> {
    let email = email.trim().to_ascii_lowercase();
    let ok = (3..=254).contains(&email.len())
        && email.split('@').count() == 2
        && !email.starts_with('@')
        && !email.ends_with('@')
        && !email.contains(|c: char| c.is_whitespace() || c.is_control() || c == ',');
    if !ok {
        return Err(format!("{email:?} is not an email address"));
    }
    policy
        .members
        .retain(|e, _| !e.eq_ignore_ascii_case(&email));
    if let Some(role) = role {
        policy.members.insert(email, role);
    }
    Ok(policy)
}

/// Who a request on the apps listener is, from its Access JWT (None: no JWT header;
/// Some(Err): one that didn't verify for the apps AUD), whether it carries Cloudflare's
/// headers, and whether its TCP peer is loopback. None: refused.
///
/// Without a JWT only a direct loopback request passes, as `local`: cloudflared connects
/// from loopback too, but always adds Cloudflare's headers.
pub fn identify(
    jwt: Option<Result<String, ()>>,
    via_cloudflare: bool,
    loopback: bool,
) -> Option<Subject> {
    match jwt {
        Some(Ok(who)) => Some(Subject::User(who.to_ascii_lowercase())),
        Some(Err(())) => None,
        None => (loopback && !via_cloudflare).then_some(Subject::Local),
    }
}

/// `https://home.example.com` -> `home.example.com`; a port only when it isn't the scheme's
/// default. Err: not an http(s) URL with a host.
pub fn public_authority(url: &str) -> Result<String, String> {
    let bad = || format!("{url:?}: expected https://host[:port]");
    let uri: axum::http::Uri = url.parse().map_err(|_| bad())?;
    let default = match uri.scheme_str() {
        Some("https") => 443,
        Some("http") => 80,
        _ => return Err(bad()),
    };
    let a = uri.authority().ok_or_else(bad)?;
    if a.as_str().contains('@') || a.host().is_empty() {
        return Err(bad());
    }
    let host = a.host().to_ascii_lowercase();
    Ok(match a.port_u16().filter(|p| *p != default) {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// The apps listener answers only to its public name ([`public_authority`]) or a loopback
/// name, so a DNS-rebound name (evil.com -> 127.0.0.1) gets nothing.
pub fn host_allowed(host: Option<&str>, public: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    public.is_some_and(|p| host.eq_ignore_ascii_case(p)) || crate::loopback_host(host)
}

/// `/a/pool/x.js` -> `pool`; `/a/` and other paths -> None.
pub fn app_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/a/")?;
    let slug = rest.split('/').next()?;
    (!slug.is_empty()).then_some(slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use AppRole::*;

    fn policy(members: &[(&str, AppRole)]) -> Policy {
        let members = members.iter().map(|(e, r)| (e.to_string(), *r)).collect();
        Policy { members }
    }

    #[test]
    fn roles_match_email_ignoring_case_and_local_owns_everything() {
        let p = policy(&[("me@example.com", Owner), ("Spouse@Example.com", Viewer)]);
        let user = |e: &str| Subject::User(e.into());
        assert_eq!(role_of(&p, &user("me@example.com")), Some(Owner));
        assert_eq!(role_of(&p, &user("spouse@example.com")), Some(Viewer));
        assert_eq!(role_of(&p, &user("other@example.com")), None);
        assert_eq!(role_of(&Policy::default(), &Subject::Local), Some(Owner));
        assert_eq!(role_of(&Policy::default(), &user("me@example.com")), None);
    }

    #[test]
    fn viewers_read_editors_write_owners_admin() {
        let all = [Action::Read, Action::Write, Action::Admin];
        let granted = |r| all.iter().filter(|a| allows(r, **a)).count();
        assert_eq!(
            (granted(Owner), granted(Editor), granted(Viewer)),
            (3, 2, 1)
        );
        assert!(allows(Editor, Action::Write) && !allows(Editor, Action::Admin));
        assert!(allows(Viewer, Action::Read) && !allows(Viewer, Action::Write));
    }

    #[test]
    fn set_member_normalises_replaces_and_removes() {
        let p = set_member(Policy::default(), " Me@Example.com ", Some(Editor)).unwrap();
        assert_eq!(p, policy(&[("me@example.com", Editor)]));
        // A hand-edited mixed-case entry is replaced, not duplicated.
        let p = set_member(
            policy(&[("ME@example.com", Viewer)]),
            "me@example.com",
            Some(Owner),
        );
        assert_eq!(p.unwrap(), policy(&[("me@example.com", Owner)]));
        let p = set_member(
            policy(&[("me@example.com", Viewer)]),
            "ME@example.com",
            None,
        );
        assert_eq!(p.unwrap(), Policy::default());
        // Removing someone absent is fine.
        assert_eq!(
            set_member(Policy::default(), "a@b", None).unwrap(),
            Policy::default()
        );
        for bad in [
            "",
            "me",
            "@example.com",
            "me@",
            "a@b@c",
            "a b@c",
            "a@b,c@d",
            "local",
        ] {
            assert!(
                set_member(Policy::default(), bad, Some(Viewer)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn policy_file_shape() {
        let p: Policy =
            serde_json::from_str(r#"{"members":{"me@example.com":"owner","k@x.y":"viewer"}}"#)
                .unwrap();
        assert_eq!(p, policy(&[("k@x.y", Viewer), ("me@example.com", Owner)]));
        assert_eq!(
            serde_json::from_str::<Policy>("{}").unwrap(),
            Policy::default()
        );
        assert!(serde_json::from_str::<Policy>(r#"{"members":{"a@b":"admin"}}"#).is_err());
    }

    #[test]
    fn only_a_verified_jwt_or_a_direct_loopback_request_gets_in() {
        let jwt = |s: &str| Some(Ok(s.to_string()));
        let me = Some(Subject::User("me@example.com".into()));
        assert_eq!(identify(jwt("Me@Example.com"), true, true), me);
        assert_eq!(identify(jwt("me@example.com"), false, false), me);
        assert_eq!(identify(Some(Err(())), true, true), None);
        assert_eq!(identify(Some(Err(())), false, true), None);
        assert_eq!(identify(None, false, true), Some(Subject::Local));
        // Through the tunnel without a JWT (Access not in front), or from the LAN.
        assert_eq!(identify(None, true, true), None);
        assert_eq!(identify(None, false, false), None);
    }

    #[test]
    fn public_authority_drops_default_ports_and_refuses_non_urls() {
        let a = |u| public_authority(u);
        assert_eq!(a("https://Home.Example.com").unwrap(), "home.example.com");
        assert_eq!(
            a("https://home.example.com:443/").unwrap(),
            "home.example.com"
        );
        assert_eq!(a("http://127.0.0.1:7475").unwrap(), "127.0.0.1:7475");
        assert_eq!(a("https://h.example:8443").unwrap(), "h.example:8443");
        for bad in ["home.example.com", "ftp://h", "https://u@h", "https://", ""] {
            assert!(a(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_must_be_the_public_name_or_loopback() {
        let public = Some("home.example.com");
        assert!(host_allowed(Some("home.example.com"), public));
        assert!(host_allowed(Some("HOME.example.com"), public));
        for h in [
            "localhost",
            "localhost:7475",
            "127.0.0.1:7475",
            "[::1]:7475",
        ] {
            assert!(host_allowed(Some(h), public), "{h}");
            assert!(host_allowed(Some(h), None), "{h}");
        }
        for h in [
            "evil.com",
            "home.example.com.evil.com",
            "home.example.com:8443",
            "192.168.1.5:7475",
        ] {
            assert!(!host_allowed(Some(h), public), "{h}");
        }
        assert!(!host_allowed(Some("home.example.com"), None));
        assert!(!host_allowed(None, public));
    }

    #[test]
    fn app_of_names_the_slug() {
        assert_eq!(app_of("/a/pool/"), Some("pool"));
        assert_eq!(app_of("/a/pool"), Some("pool"));
        assert_eq!(app_of("/a/pool/api/v1/me"), Some("pool"));
        assert_eq!(app_of("/a/"), None);
        assert_eq!(app_of("/a"), None);
        assert_eq!(app_of("/nope"), None);
    }
}
