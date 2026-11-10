//! Tasks that build or change an app (`+ App`, `Change…`): the prompt they run with. Pure.

use mis_types::{
    check_references, guides, references_text, valid_slug, AppTask, WorkerId, SLUG_RULE,
};

use crate::app_grant::Binding;

/// What `t` asks for, its links and notes, then the apps guide. Err: why it can't be a task.
pub fn prompt(t: &AppTask) -> Result<String, String> {
    let request = t.request.trim();
    if request.is_empty() {
        return Err("say what the app should do".into());
    }
    let slug = slug(t);
    if let Some(s) = slug.filter(|s| !valid_slug(s)) {
        return Err(format!("{s:?}: {SLUG_RULE}"));
    }
    let what = match (t.change, slug) {
        (true, Some(s)) => format!("Change the Make It So app `{s}` (`apps/{s}/`)."),
        (true, None) => return Err("say which app to change".into()),
        (false, Some(s)) => format!("Build a new Make It So app, slug `{s}`."),
        (false, None) => "Build a new Make It So app; pick its slug.".into(),
    };
    check_references(&t.references)?;
    let refs = match references_text(&t.references) {
        r if r.is_empty() => r,
        r => format!("{r}\n\n"),
    };
    let guide = guides::guide("apps").expect("the apps guide is compiled in");
    Ok(format!(
        "{what} This folder is a checkout of the apps repo. Follow the apps guide below to \
         the end: a PR and a staged release, never live.\n\nRequest:\n{request}\n\n{refs}{guide}"
    ))
}

/// The app `t`'s token may stage releases of (`t` passed [`prompt`]): a change, its app;
/// `+ App`, one new app (the slug named, else any), owned by `requester`.
pub fn binding(t: &AppTask, requester: Option<String>) -> Binding {
    Binding {
        thread: uuid::Uuid::nil(),
        slug: slug(t).map(Into::into),
        create: !t.change,
        requester,
    }
}

/// The worker an app task runs on: the one it `picked`, else Settings' (`set`), else an
/// `online` worker (id, hostname; those running its harness) on the orchestrator's `host`, else
/// any (None). By hostname, as the worker reports it: the same machine names itself the same.
pub fn machine<'a>(
    picked: Option<WorkerId>,
    set: Option<WorkerId>,
    host: &str,
    online: impl IntoIterator<Item = (WorkerId, &'a str)>,
) -> Option<WorkerId> {
    let here = |h: &str| !host.is_empty() && h.eq_ignore_ascii_case(host);
    let local = (online.into_iter())
        .filter(|(_, h)| here(h))
        .map(|(id, _)| id);
    // Lowest id: the same one each time, whatever the order.
    picked.or(set).or_else(|| local.min())
}

/// The app `t` names, if any.
fn slug(t: &AppTask) -> Option<&str> {
    t.slug.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(request: &str, slug: Option<&str>, change: bool) -> AppTask {
        AppTask {
            request: request.into(),
            slug: slug.map(Into::into),
            change,
            harness: "claude".into(),
            model: None,
            worker: None,
            images: vec![],
            references: vec![],
        }
    }

    #[test]
    fn a_new_app_gets_the_request_then_the_guide() {
        let p = prompt(&task(" Track pool chlorine. \n", Some(" pool "), false)).unwrap();
        assert!(
            p.starts_with("Build a new Make It So app, slug `pool`."),
            "{p}"
        );
        assert!(p.contains("\n\nRequest:\nTrack pool chlorine.\n\n---\nname: apps\n"));
        assert!(p.ends_with(guides::guide("apps").unwrap()));
        let p = prompt(&task("Chores.", None, false)).unwrap();
        assert!(p.starts_with("Build a new Make It So app; pick its slug."));
        assert_eq!(prompt(&task("Chores.", Some(""), false)).unwrap(), p);
    }

    #[test]
    fn a_change_is_bound_to_its_app_and_a_new_one_may_create_one() {
        let me = Some("me@example.com".to_string());
        let b = binding(&task("x", Some(" pool "), true), me.clone());
        assert_eq!((b.slug.as_deref(), b.create), (Some("pool"), false));
        let b = binding(&task("x", Some("pool"), false), me.clone());
        assert_eq!((b.slug.as_deref(), b.create), (Some("pool"), true));
        let b = binding(&task("x", Some(""), false), me.clone());
        assert_eq!((b.slug, b.create, b.requester), (None, true, me));
    }

    #[test]
    fn the_machine_is_the_picked_then_the_set_then_this_hosts_then_any() {
        let [a, b, c] = [1, 2, 3].map(uuid::Uuid::from_u128);
        let online = [(c, "studio"), (b, "Demo-Mac"), (a, "demo-mac")];
        assert_eq!(machine(Some(c), Some(b), "demo-mac", online), Some(c));
        assert_eq!(machine(None, Some(c), "demo-mac", online), Some(c));
        assert_eq!(machine(None, None, "demo-mac", online), Some(a));
        assert_eq!(machine(None, None, "laptop", online), None);
        assert_eq!(machine(None, None, "", [(a, "")]), None);
    }

    #[test]
    fn links_and_notes_go_between_the_request_and_the_guide() {
        let mut t = task("Chores for the kids.", None, false);
        t.references = vec![
            mis_types::Reference::Link {
                url: "https://example.com/grid".into(),
                title: None,
            },
            mis_types::Reference::Note {
                text: "10 stars = $1".into(),
            },
        ];
        let p = prompt(&t).unwrap();
        assert!(p.contains(
            "Request:\nChores for the kids.\n\nReferences:\n- Link: <https://example.com/grid>\n\
             - Note:\n  > 10 stars = $1\n\n---\nname: apps\n"
        ));
        t.references.push(mis_types::Reference::Link {
            url: "javascript:x".into(),
            title: None,
        });
        assert!(prompt(&t).is_err());
    }

    #[test]
    fn a_change_names_the_app() {
        let p = prompt(&task("Add a pH chart.", Some("pool"), true)).unwrap();
        assert!(p.starts_with("Change the Make It So app `pool` (`apps/pool/`)."));
        assert!(p.contains("Request:\nAdd a pH chart.\n"));
    }

    #[test]
    fn blank_requests_bad_slugs_and_changes_to_no_app_are_refused() {
        assert!(prompt(&task("  ", Some("pool"), false)).is_err());
        assert!(prompt(&task("x", Some("Pool!"), false))
            .unwrap_err()
            .contains("a-z"));
        assert!(prompt(&task("x", Some("../x"), true)).is_err());
        assert!(prompt(&task("x", None, true))
            .unwrap_err()
            .contains("which app"));
    }
}
