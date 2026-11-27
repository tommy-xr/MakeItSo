//! Tasks that write a routine the owner describes (Routines `+`): the prompt they run with. Pure.

use mis_types::guides;

use crate::plan::Card;

/// What the owner asked for, the workers a routine may name, then the routines guide.
/// Err: why it can't be a task.
pub fn prompt(request: &str, workers: &[Card]) -> Result<String, String> {
    let request = request.trim();
    if request.is_empty() {
        return Err("say what the routine should do, and when".into());
    }
    let workers: String = (workers.iter())
        .map(|c| {
            let off = if c.online { "" } else { " (offline)" };
            let purpose = c.purpose.as_ref().map(|p| format!(": {p}"));
            format!("- {}{off}{}\n", c.name, purpose.unwrap_or_default())
        })
        .collect();
    let guide = guides::guide("routines").expect("the routines guide is compiled in");
    Ok(format!(
        "Write a Make It So routine for the owner, following the routines guide below: a \
         prompt or a script, its schedule, worker and cwd, `act: true` only if it must change \
         things, and `paused: true`. Write the file outside this checkout, then send it with \
         `mis routines write <file>`; your token writes only this one routine. Never run, \
         resume or approve it: the owner does. Finish by saying what it does and why you \
         chose its schedule, worker and mode.\n\nRequest:\n{request}\n\n\
         Workers (`worker:` takes a name):\n{workers}\n{guide}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(name: &str, online: bool, purpose: Option<&str>) -> Card {
        Card {
            id: uuid::Uuid::nil(),
            name: name.into(),
            online,
            harnesses: vec!["claude".into()],
            purpose: purpose.map(Into::into),
            tools: String::new(),
        }
    }

    #[test]
    fn the_request_then_the_workers_then_the_guide() {
        let workers = [
            card("demo-mac", true, Some("builds")),
            card("mini", false, None),
        ];
        let p = prompt(" Mirror my repos every 6h. \n", &workers).unwrap();
        assert!(p.starts_with("Write a Make It So routine"), "{p}");
        assert!(p.contains("`mis routines write <file>`"));
        assert!(p.contains(
            "\n\nRequest:\nMirror my repos every 6h.\n\nWorkers (`worker:` takes a name):\n\
             - demo-mac: builds\n- mini (offline)\n\n---\nname: routines\n"
        ));
        assert!(p.ends_with(guides::guide("routines").unwrap()));
    }

    #[test]
    fn a_blank_request_is_refused() {
        assert!(prompt(" \n", &[]).unwrap_err().contains("what the routine"));
    }
}
