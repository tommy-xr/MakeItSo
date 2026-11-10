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
    /// The owner is trying the app's draft, this release: its files and a copy of the records.
    pub draft: Option<u32>,
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
    /// The app task thread that created the app, which may go on staging its releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<uuid::Uuid>,
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

/// The cookie that puts `/a/<slug>/` in draft mode for an owner or editor. Only a selector (each
/// request is checked again), so not HttpOnly: `mis.js` reads it to say it's a draft.
pub fn draft_cookie_name(slug: &str) -> String {
    format!("mis-draft-{slug}")
}

/// Header `mis.js` sends on each API call from a page loaded as a draft: its release. The
/// API goes by it, not the cookie, so a page left open after the draft ends (or after another
/// tab left it) can't write to the live records.
pub const DRAFT_HEADER: &str = "x-mis-draft";

/// The draft release `Cookie` header `cookies` names for `slug`, if any.
pub fn cookie_draft(cookies: Option<&str>, slug: &str) -> Option<u32> {
    let name = draft_cookie_name(slug);
    (cookies?.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)?
        .1
        .parse()
        .ok()
}

/// What a request gets of an app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Serve {
    Live,
    /// Its owner trying draft release n.
    Draft(u32),
    /// An API call for a draft that no longer applies (replaced, decided, not the owner): it
    /// must not fall back to live data.
    Gone,
}

/// Whether `role` may try an app's draft: owners and editors (viewers see only the live app).
pub fn may_try(role: AppRole) -> bool {
    allows(role, Action::Write)
}

/// What a request asking for draft `asked` (cookie or header) gets, `current` being the app's
/// draft now.
pub fn draft_of(asked: Option<u32>, api: bool, role: AppRole, current: Option<u32>) -> Serve {
    match asked {
        Some(n) if may_try(role) && current == Some(n) => Serve::Draft(n),
        Some(_) if api => Serve::Gone,
        _ => Serve::Live,
    }
}

/// `Set-Cookie` entering draft `n` of `slug`, or (None) leaving it. `secure` over HTTPS.
pub fn draft_cookie(slug: &str, n: Option<u32>, secure: bool) -> String {
    let (value, age) = match n {
        Some(n) => (n.to_string(), ""),
        None => (String::new(), "; Max-Age=0"),
    };
    let secure = if secure { "; Secure" } else { "" };
    let name = draft_cookie_name(slug);
    format!("{name}={value}; Path=/a/{slug}/; SameSite=Lax{age}{secure}")
}

/// Header a `local` request sends, under `--apps-dev`, to act as `viewer` or `editor`.
pub const DEV_ROLE: &str = "x-mis-dev-role";

/// [`DEV_ROLE`]'s value: a role below owner, which `local` already is, so it only lowers.
pub fn dev_role(v: &str) -> Option<AppRole> {
    match v {
        "viewer" => Some(AppRole::Viewer),
        "editor" => Some(AppRole::Editor),
        _ => None,
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

/// `raw` as a member's email: trimmed, lower-case. None: not an email address.
pub fn email(raw: &str) -> Option<String> {
    let email = raw.trim().to_ascii_lowercase();
    let ok = (3..=254).contains(&email.len())
        && email.split('@').count() == 2
        && !email.starts_with('@')
        && !email.ends_with('@')
        && !email.contains(|c: char| c.is_whitespace() || c.is_control() || c == ',');
    ok.then_some(email)
}

/// `policy` with `email` given `role`, or removed for None. Err: not an email.
pub fn set_member(
    mut policy: Policy,
    email: &str,
    role: Option<AppRole>,
) -> Result<Policy, String> {
    let Some(email) = self::email(email) else {
        let email = email.trim().to_ascii_lowercase();
        return Err(format!("{email:?} is not an email address"));
    };
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

/// A write (any method but GET and HEAD) from a browser only from the apps' own page: a
/// JSON body (no HTML form can send one, and fetch can't cross origins with it unasked),
/// and an `Origin`, if any, of `https://<public>` or the loopback name it was sent to.
/// No `Origin`: not a browser (e.g. a functor client). Matters since loopback is owner.
pub fn write_allowed(
    content_type: Option<&str>,
    origin: Option<&str>,
    host: Option<&str>,
    public: Option<&str>,
) -> bool {
    let json = content_type.is_some_and(|t| {
        let mime = t.split(';').next().unwrap_or("").trim();
        mime.eq_ignore_ascii_case("application/json")
    });
    let from_here = origin.is_none_or(|o| {
        let public_page = o.strip_prefix("https://").zip(public);
        public_page.is_some_and(|(a, p)| a.eq_ignore_ascii_case(p))
            || host.is_some_and(|h| crate::loopback_host(h) && crate::same_origin(o, h))
    });
    json && from_here
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

    #[test]
    fn owners_and_editors_try_the_current_draft_named_by_its_cookie() {
        use AppRole::*;
        let c = Some("a=1; mis-draft-pool=3; mis-draft-budget=7");
        assert_eq!(cookie_draft(c, "pool"), Some(3));
        assert_eq!(cookie_draft(c, "chores"), None);
        assert_eq!(cookie_draft(Some("mis-draft-pool=x"), "pool"), None);
        assert_eq!(cookie_draft(None, "pool"), None);
        // Pages fall back to live; the API refuses rather than touch live data.
        for api in [false, true] {
            assert_eq!(draft_of(Some(3), api, Owner, Some(3)), Serve::Draft(3));
            assert_eq!(draft_of(None, api, Owner, Some(3)), Serve::Live);
        }
        assert_eq!(draft_of(Some(3), true, Editor, Some(3)), Serve::Draft(3));
        for (role, current) in [(Viewer, Some(3)), (Owner, Some(4)), (Owner, None)] {
            assert_eq!(draft_of(Some(3), false, role, current), Serve::Live);
            assert_eq!(draft_of(Some(3), true, role, current), Serve::Gone);
        }
        assert_eq!(
            draft_cookie("pool", Some(3), true),
            "mis-draft-pool=3; Path=/a/pool/; SameSite=Lax; Secure"
        );
        assert_eq!(
            draft_cookie("pool", None, false),
            "mis-draft-pool=; Path=/a/pool/; SameSite=Lax; Max-Age=0"
        );
    }
    use AppRole::*;

    fn policy(members: &[(&str, AppRole)]) -> Policy {
        let members = members.iter().map(|(e, r)| (e.to_string(), *r)).collect();
        Policy {
            members,
            ..Default::default()
        }
    }

    #[test]
    fn dev_roles_only_lower() {
        assert_eq!(dev_role("viewer"), Some(Viewer));
        assert_eq!(dev_role("editor"), Some(Editor));
        for bad in ["owner", "Viewer", "", "admin"] {
            assert_eq!(dev_role(bad), None, "{bad}");
        }
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
    fn writes_need_json_from_the_apps_own_origin() {
        let json = Some("application/json");
        let public = Some("home.example.com");
        let home = Some("home.example.com");
        let w = |ct, origin, host| write_allowed(ct, origin, host, public);
        assert!(w(json, Some("https://home.example.com"), home));
        assert!(w(
            Some("Application/JSON; charset=utf-8"),
            Some("https://HOME.example.com"),
            home
        ));
        // cloudflared may hand on a loopback Host; the public origin still counts.
        assert!(w(
            json,
            Some("https://home.example.com"),
            Some("localhost:7475")
        ));
        assert!(w(
            json,
            Some("http://127.0.0.1:7475"),
            Some("127.0.0.1:7475")
        ));
        assert!(w(
            json,
            Some("http://localhost:7475"),
            Some("localhost:7475")
        ));
        assert!(w(json, None, home)); // not a browser
        for ct in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=x"),
            Some("application/jsonx"),
        ] {
            assert!(!w(ct, Some("https://home.example.com"), home), "{ct:?}");
        }
        for origin in [
            "https://evil.example",
            "http://home.example.com", // not https
            "https://home.example.com.evil.example",
            "http://localhost:8080", // another local page
            "null",
            "",
        ] {
            assert!(!w(json, Some(origin), Some("localhost:7475")), "{origin}");
            assert!(!w(json, Some(origin), home), "{origin}");
        }
        assert!(!write_allowed(
            json,
            Some("https://home.example.com"),
            home,
            None
        ));
    }

    #[test]
    fn app_of_names_the_slug() {
        assert_eq!(app_of("/a/pool/"), Some("pool"));
        assert_eq!(app_of("/a/pool"), Some("pool"));
        assert_eq!(app_of("/a/pool/api/v1/me"), Some("pool"));
        // Raw, not decoded: an encoded slug isn't a valid one, so it gets no member's role.
        assert_eq!(app_of("/a/po%6Fl/"), Some("po%6Fl"));
        assert!(!crate::apps::valid_slug("po%6Fl"));
        assert_eq!(app_of("/a/"), None);
        assert_eq!(app_of("/a"), None);
        assert_eq!(app_of("/nope"), None);
    }
}
