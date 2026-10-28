//! Read-only MCP tools over the fleet, for `mis ask`. Pure: every answer comes
//! from a ledger snapshot; the shell serves them at `/mcp`.

use mis_types::*;
use serde_json::{json, Value};

use crate::ledger::Ledger;
use crate::summary;

/// `list_tasks` returns at most this many, newest first.
const MAX_TASKS: usize = 50;
/// `show_task` keeps the latest transcript lines within this many chars.
const MAX_TRANSCRIPT_CHARS: usize = 8_000;
/// Prompt preview in `list_tasks`.
const PROMPT_CHARS: usize = 160;

/// The `tools/list` payload.
pub fn list() -> Vec<Value> {
    let none = json!({"type": "object", "properties": {}});
    vec![
        json!({
            "name": "list_workers",
            "description": "Machines in the fleet: hostname, harnesses, roots, online/busy, load, and the task each is running.",
            "inputSchema": none,
        }),
        json!({
            "name": "list_tasks",
            "description": "Recent tasks, newest first: id, status, harness/model/machine, elapsed, current activity, prompt preview and outcome summary.",
            "inputSchema": {
                "type": "object",
                "properties": {"status": {
                    "type": "string",
                    "enum": ["planning", "queued", "running", "done", "failed"],
                    "description": "Only tasks in this state.",
                }},
            },
        }),
        json!({
            "name": "show_task",
            "description": "One task in full: prompt, status, summary, session, and its latest transcript.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string", "description": "Task id or unambiguous prefix, e.g. 81df88c0."}},
                "required": ["id"],
            },
        }),
    ]
}

/// Run tool `name` against the ledger. Err is shown to the model as a failed call.
pub fn call(l: &Ledger, name: &str, args: &Value) -> Result<Value, String> {
    let ServerMsg::Status {
        workers,
        tasks,
        now_ms,
    } = l.status()
    else {
        unreachable!("status() returns Status")
    };
    let now = now_ms.unwrap_or_default();
    let host = |wid: Option<WorkerId>| {
        let wid = wid?;
        let w = workers.iter().find(|w| w.id == wid);
        Some(w.map_or_else(|| wid.to_string(), |w| w.info.hostname.clone()))
    };
    let elapsed = |t: &TaskSummary| {
        t.started_at
            .map(|s| fmt_elapsed(t.finished_at.unwrap_or(now).saturating_sub(s)))
    };
    match name {
        "list_workers" => {
            let mut ws: Vec<_> = workers.iter().collect();
            ws.sort_by(|a, b| a.info.hostname.cmp(&b.info.hostname));
            Ok(ws
                .into_iter()
                .map(|w| {
                    let running = tasks
                        .iter()
                        .find(|t| t.worker == Some(w.id) && t.status == TaskStatus::Running);
                    json!({
                        "id": w.id,
                        "hostname": w.info.hostname,
                        "os": format!("{} {}", w.info.os, w.info.arch),
                        "harnesses": w.info.harnesses,
                        "roots": w.info.roots,
                        "online": w.online,
                        "busy": w.busy,
                        "load": w.stats.as_ref().map(|s| json!({
                            "cpu_pct": s.cpu_pct, "mem_pct": s.mem_pct, "disk_used_pct": s.disk_used_pct,
                        })),
                        "running": running.map(|t| json!({"id": t.id, "activity": t.activity})),
                    })
                })
                .collect())
        }
        "list_tasks" => {
            let want: Option<TaskStatus> = match &args["status"] {
                Value::Null => None,
                s => Some(
                    serde_json::from_value(s.clone()).map_err(|_| format!("unknown status {s}"))?,
                ),
            };
            Ok(tasks
                .iter()
                .rev()
                .filter(|t| want.is_none_or(|s| t.status == s))
                .take(MAX_TASKS)
                .map(|t| {
                    json!({
                        "id": t.id,
                        "status": t.status,
                        "harness": t.harness,
                        "model": t.model,
                        "machine": host(t.worker),
                        "elapsed": elapsed(t),
                        "activity": t.activity,
                        "prompt": first_line(&t.prompt, PROMPT_CHARS),
                        "summary": t.summary,
                    })
                })
                .collect())
        }
        "show_task" => {
            let prefix = args["id"].as_str().unwrap_or("").trim();
            let hits: Vec<_> = tasks
                .iter()
                .filter(|t| !prefix.is_empty() && t.id.to_string().starts_with(prefix))
                .collect();
            let id = match hits[..] {
                [t] => t.id,
                [] => return Err(format!("no task matches {prefix:?}")),
                _ => return Err(format!("{prefix:?} matches {} tasks", hits.len())),
            };
            let (t, events, _) = l.show(id).expect("listed task exists");
            Ok(json!({
                "id": t.id,
                "status": t.status,
                "harness": t.harness,
                "model": t.model,
                "machine": host(t.worker),
                "session": t.session,
                "elapsed": elapsed(&t),
                "tokens": t.tokens_in.zip(t.tokens_out).map(|(i, o)| json!({"in": i, "out": o})),
                "prompt": t.prompt,
                "summary": t.summary,
                "transcript": summary::transcript(events, MAX_TRANSCRIPT_CHARS),
            }))
        }
        _ => Err(format!("unknown tool {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// One worker `mini`; task A done with a transcript, task B queued.
    fn fleet() -> (Ledger, TaskId, TaskId) {
        let mut l = Ledger::default();
        l.now_ms = 1_000;
        let wid = uuid::Uuid::new_v4();
        let info = WorkerInfo {
            hostname: "mini".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            harnesses: vec!["claude".into()],
            roots: vec!["/code".into()],
            models: Default::default(),
        };
        l.connect(wid, info, &[], 1, Duration::ZERO);
        let (a, _) = l.submit("count files".into(), None, Some("claude".into()), None);
        for e in [
            AgentEvent::Started {
                session: Some("s1".into()),
                model: Some("claude-haiku".into()),
            },
            AgentEvent::Text {
                text: "Three.".into(),
            },
            AgentEvent::Finished {
                ok: true,
                summary: Some("Three files.".into()),
            },
        ] {
            l.now_ms = 43_000;
            l.event(wid, 1, a, e);
        }
        let (b, _) = l.submit("second".into(), None, Some("codex".into()), None);
        (l, a, b)
    }

    #[test]
    fn lists_workers_and_tasks() {
        let (l, a, b) = fleet();
        let ws = call(&l, "list_workers", &json!({})).unwrap();
        assert_eq!(ws[0]["hostname"], "mini");
        assert_eq!(
            (ws[0]["online"].clone(), ws[0]["busy"].clone()),
            (json!(true), json!(false))
        );
        let ts = call(&l, "list_tasks", &json!({})).unwrap();
        assert_eq!(ts[0]["id"], json!(b), "newest first");
        assert_eq!(ts[1]["machine"], "mini");
        assert_eq!(ts[1]["elapsed"], "42s");
        assert_eq!(ts[1]["summary"], "Three files.");
        let done = call(&l, "list_tasks", &json!({"status": "done"})).unwrap();
        assert_eq!(done.as_array().unwrap().len(), 1);
        assert_eq!(done[0]["id"], json!(a));
        assert!(call(&l, "list_tasks", &json!({"status": "bogus"})).is_err());
    }

    #[test]
    fn shows_task_by_prefix() {
        let (l, a, _) = fleet();
        let t = call(&l, "show_task", &json!({"id": &a.to_string()[..8]})).unwrap();
        assert_eq!(t["prompt"], "count files");
        assert_eq!(t["session"], "s1");
        assert_eq!(t["transcript"], "Three.\ndone");
        assert!(call(&l, "show_task", &json!({"id": "zzz"})).is_err());
        assert!(call(&l, "show_task", &json!({"id": ""})).is_err());
        assert!(call(&l, "show_task", &json!({})).is_err());
        assert!(call(&l, "nope", &json!({})).is_err());
        let names: Vec<_> = list().iter().map(|t| t["name"].clone()).collect();
        assert_eq!(names, ["list_workers", "list_tasks", "show_task"]);
    }
}
