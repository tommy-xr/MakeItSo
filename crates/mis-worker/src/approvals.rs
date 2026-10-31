//! Ask mode's shell: claude's `approve` calls wait here for the user's answer,
//! which arrives from the orchestrator; "always" saves its rule to worker.toml.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mis_types::{Approval, Decision, Permissions, TaskId, WorkerMsg};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{config, rules};

struct Pending {
    task: TaskId,
    approval: Approval,
    rule: Option<rules::Rule>,
    reply: oneshot::Sender<Decision>,
}

pub struct Approvals {
    pending: Mutex<HashMap<Uuid, Pending>>,
    /// The worker's permissions; "always" adds to them.
    pub permissions: Arc<Mutex<Option<Permissions>>>,
    /// Requests and closes go to the orchestrator through here.
    out: mpsc::Sender<WorkerMsg>,
    /// worker.toml, where "always" saves its rule.
    config: PathBuf,
    home: Option<PathBuf>,
    wait: Duration,
}

/// The JSON text claude's permission prompt tool contract expects back.
fn allow(input: &Value) -> String {
    json!({"behavior": "allow", "updatedInput": input}).to_string()
}

fn deny(message: &str) -> String {
    json!({"behavior": "deny", "message": message}).to_string()
}

impl Approvals {
    pub fn new(
        permissions: Option<Permissions>,
        out: mpsc::Sender<WorkerMsg>,
        config: PathBuf,
        home: Option<PathBuf>,
        wait: Duration,
    ) -> Self {
        Self {
            pending: Default::default(),
            permissions: Arc::new(Mutex::new(permissions)),
            out,
            config,
            home,
            wait,
        }
    }

    /// One `approve` call (`{tool_name, input, tool_use_id}`) from `task`'s
    /// agent: allowed at once if a rule saved since the run began covers it,
    /// else asked of the user and waited on; no answer in `wait` is a deny.
    pub async fn ask(&self, task: TaskId, args: &Value) -> String {
        let tool = args["tool_name"].as_str().unwrap_or_default();
        let input = &args["input"];
        let saved = self.permissions.lock().unwrap().clone();
        if saved.is_some_and(|p| rules::covered(&p, tool, input)) {
            return allow(input);
        }
        let home = self.home.as_deref();
        let rule = rules::always(tool, input, home);
        let approval = Approval {
            id: Uuid::new_v4(),
            tool: tool.to_string(),
            summary: rules::summary(tool, input, home),
            detail: rules::detail(tool, input),
            always: rule.as_ref().map(|r| rules::describe(r, home)),
        };
        let id = approval.id;
        let (reply, answer) = oneshot::channel();
        let request = WorkerMsg::ApprovalRequest {
            task,
            approval: approval.clone(),
        };
        self.pending.lock().unwrap().insert(
            id,
            Pending {
                task,
                approval,
                rule,
                reply,
            },
        );
        tracing::info!(%task, %id, "asking the user: {tool}");
        let _ = self.out.send(request).await;
        match tokio::time::timeout(self.wait, answer).await {
            Ok(Ok(Decision::Allow | Decision::Always)) => allow(input),
            Ok(Ok(Decision::Deny)) => deny("the user denied this"),
            Ok(Err(_)) => deny("the request was dropped"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                let _ = self.out.send(WorkerMsg::ApprovalClosed { task, id }).await;
                deny("no answer from the user")
            }
        }
    }

    /// The user's answer to `id` (unknown ids are ignored). Always: its rule is
    /// saved to worker.toml and applied, and the new permissions returned.
    pub fn decide(&self, id: Uuid, decision: Decision) -> Option<Permissions> {
        let p = self.pending.lock().unwrap().remove(&id)?;
        let saved = match (decision, &p.rule) {
            (Decision::Always, Some(rule)) => match self.save(rule) {
                Ok(perms) => perms,
                Err(e) => {
                    tracing::warn!(task = %p.task, "allowing once; saving the rule failed: {e:#}");
                    None
                }
            },
            _ => None,
        };
        let _ = p.reply.send(decision);
        saved
    }

    /// Add `rule` to worker.toml's `[permissions]` and to the live ones.
    fn save(&self, rule: &rules::Rule) -> anyhow::Result<Option<Permissions>> {
        let mut file = config::load(&self.config)?;
        let mut on_disk = file.permissions.take().unwrap_or_default();
        rules::add(&mut on_disk, rule);
        file.permissions = Some(on_disk);
        config::write(&self.config, &file)?;
        let mut live = self.permissions.lock().unwrap();
        if let Some(p) = live.as_mut() {
            rules::add(p, rule);
        }
        tracing::info!(?rule, "saved to {}", self.config.display());
        Ok(live.clone())
    }

    /// Requests still waiting, to send again after a reconnect.
    pub fn waiting(&self) -> Vec<WorkerMsg> {
        let pending = self.pending.lock().unwrap();
        let mut v: Vec<&Pending> = pending.values().collect();
        v.sort_by_key(|p| p.approval.id);
        v.into_iter()
            .map(|p| WorkerMsg::ApprovalRequest {
                task: p.task,
                approval: p.approval.clone(),
            })
            .collect()
    }

    pub fn is_waiting(&self, id: Uuid) -> bool {
        self.pending.lock().unwrap().contains_key(&id)
    }

    /// `task` ended: drop what it was waiting on.
    pub fn forget(&self, task: TaskId) {
        self.pending.lock().unwrap().retain(|_, p| p.task != task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn approvals(wait: Duration, dir: &Path) -> (Arc<Approvals>, mpsc::Receiver<WorkerMsg>) {
        let (tx, rx) = mpsc::channel(8);
        let perms = Permissions {
            mode: mis_types::PermissionMode::Ask,
            ..Default::default()
        };
        let path = dir.join("worker.toml");
        let a = Approvals::new(Some(perms), tx, path, Some(dir.to_path_buf()), wait);
        (Arc::new(a), rx)
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mis-approvals-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(d.join("assets")).unwrap();
        d.canonicalize().unwrap()
    }

    #[tokio::test]
    async fn unanswered_request_is_denied_and_closed() {
        let dir = tmp();
        let (a, mut rx) = approvals(Duration::from_millis(50), &dir);
        let task = Uuid::new_v4();
        let args = json!({"tool_name": "Bash", "input": {"command": "python3 x.py"}});
        let reply = a.ask(task, &args).await;
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap(),
            json!({"behavior": "deny", "message": "no answer from the user"})
        );
        let Some(WorkerMsg::ApprovalRequest { approval, .. }) = rx.recv().await else {
            panic!()
        };
        assert_eq!(approval.summary, "Bash: python3 x.py");
        assert_eq!(
            rx.recv().await,
            Some(WorkerMsg::ApprovalClosed {
                task,
                id: approval.id
            })
        );
        assert!(a.waiting().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn always_saves_the_rule_and_covers_the_next_call() {
        let dir = tmp();
        let (a, mut rx) = approvals(Duration::from_secs(5), &dir);
        let task = Uuid::new_v4();
        let file = dir.join("assets/crate.py").display().to_string();
        let args = json!({"tool_name": "Write", "input": {"file_path": file, "content": "x"}});
        let ask = tokio::spawn({
            let (a, args) = (a.clone(), args.clone());
            async move { a.ask(task, &args).await }
        });
        let Some(WorkerMsg::ApprovalRequest { approval, .. }) = rx.recv().await else {
            panic!()
        };
        assert_eq!(approval.always.as_deref(), Some("writes in ~/assets"));
        assert_eq!(a.waiting().len(), 1, "sent again on reconnect");
        assert!(a.is_waiting(approval.id));
        let perms = a.decide(approval.id, Decision::Always).unwrap();
        let assets = dir.join("assets").display().to_string();
        assert_eq!(perms.folders, std::slice::from_ref(&assets));
        let reply: Value = serde_json::from_str(&ask.await.unwrap()).unwrap();
        assert_eq!(reply["behavior"], "allow");
        assert_eq!(reply["updatedInput"], args["input"]);
        let saved = config::load(&dir.join("worker.toml")).unwrap();
        assert_eq!(saved.permissions.unwrap().folders, [assets]);
        // The next write there is allowed without asking.
        let reply: Value = serde_json::from_str(&a.ask(task, &args).await).unwrap();
        assert_eq!(reply["behavior"], "allow");
        assert!(rx.try_recv().is_err());
        assert_eq!(
            a.decide(approval.id, Decision::Deny),
            None,
            "already answered"
        );
        assert!(!a.is_waiting(approval.id));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
