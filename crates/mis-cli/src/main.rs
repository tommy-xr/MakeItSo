//! Text client for the orchestrator. Stands in for the Voice app.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use mis_types::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

#[derive(Parser)]
#[command(name = "mis", about = "Make It So CLI")]
struct Args {
    #[arg(long, env = "MIS_URL", default_value = "ws://127.0.0.1:7474")]
    url: String,
    #[arg(long, env = "MIS_TOKEN")]
    token: Option<String>,
    /// Cloudflare Access service-token id; needs --cf-access-client-secret.
    #[arg(long, env = "CF_ACCESS_CLIENT_ID")]
    cf_access_client_id: Option<String>,
    #[arg(long, env = "CF_ACCESS_CLIENT_SECRET", hide_env_values = true)]
    cf_access_client_secret: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List workers and tasks.
    Status,
    /// Submit a task.
    Task {
        prompt: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        /// Stream this task's events until it finishes.
        #[arg(long, short)]
        follow: bool,
        /// Allow submitting from inside a running task (set by MIS_TASK).
        #[arg(long)]
        nested: bool,
    },
    /// Show one task's details and transcript.
    Show {
        /// Task id or unambiguous prefix (as `mis status` prints).
        id: String,
    },
    /// Stream all task events.
    Tail,
    /// Run an orchestrator and a worker on this machine for quick testing.
    Dev {
        /// Let agents run tools without permission prompts.
        #[arg(long)]
        skip_permissions: bool,
    },
}

/// Spawns the sibling `mis-orchestrator` and `mis-worker` binaries, binding the
/// orchestrator to the host in `url` and passing `token` to both. Both are
/// killed when either exits or this process gets Ctrl-C / SIGTERM.
async fn dev(url: &str, token: Option<&str>, skip_permissions: bool) -> Result<()> {
    // Register signal handlers before spawning so a signal during startup
    // still drops (kills) the children.
    let ctrl_c = tokio::signal::ctrl_c();
    let term = terminate();
    let exe = std::env::current_exe()?;
    let bin = |name: &str| exe.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let bind = bind_addr(url).await?;
    // Children read MIS_TOKEN; override the inherited value with --token's.
    let with_token = |mut c: tokio::process::Command| match token {
        Some(t) => {
            c.env("MIS_TOKEN", t);
            c
        }
        None => {
            c.env_remove("MIS_TOKEN");
            c
        }
    };
    let orch_bin = bin("mis-orchestrator");
    let mut orch = with_token(tokio::process::Command::new(&orch_bin))
        .args(["--bind", &bind])
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {} (run `cargo build` first)", orch_bin.display()))?;
    let worker_bin = bin("mis-worker");
    let mut worker = with_token(tokio::process::Command::new(&worker_bin));
    // --root keeps tasks here rather than in a saved worker.toml's roots.
    worker
        .args(["--url", url, "--root"])
        .arg(std::env::current_dir()?)
        .kill_on_drop(true);
    if skip_permissions {
        worker.arg("--skip-permissions");
    }
    let mut worker = worker
        .spawn()
        .with_context(|| format!("starting {}", worker_bin.display()))?;
    tokio::select! {
        s = orch.wait() => bail!("mis-orchestrator exited: {}", s?),
        s = worker.wait() => bail!("mis-worker exited: {}", s?),
        _ = ctrl_c => Ok(()),
        _ = term => Ok(()),
    }
}

/// `ws://localhost:7474` -> `127.0.0.1:7474`: the orchestrator's --bind needs a
/// numeric socket address.
async fn bind_addr(url: &str) -> Result<String> {
    let host = url
        .strip_prefix("ws://")
        .with_context(|| format!("dev needs a ws:// url, got {url}"))?
        .trim_end_matches('/');
    let addrs: Vec<_> = tokio::net::lookup_host(host)
        .await
        .with_context(|| format!("resolving {host}"))?
        .collect();
    // Prefer IPv4 so `localhost` binds where clients using 127.0.0.1 expect it.
    let addr = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .with_context(|| format!("no address for {host}"))?;
    Ok(addr.to_string())
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{signal, SignalKind};
    let _ = signal(SignalKind::terminate()).unwrap().recv().await;
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending().await
}

/// An agent submitting to the fleet that is running it deadlocks on a single
/// worker, so refuse unless `--nested`. The message deliberately omits the flag:
/// agents do what error messages say.
fn check_nested(cmd: &Cmd, task_env: Option<&str>) -> Result<()> {
    match (cmd, task_env) {
        (Cmd::Task { nested: false, .. }, Some(parent)) if !parent.is_empty() => bail!(
            "refusing to submit from inside task {parent}: with one worker it can never start while this task runs. Do the work here."
        ),
        _ => Ok(()),
    }
}

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(args: &Args) -> Result<Ws> {
    let mut req = format!("{}/ws/client", args.url).into_client_request()?;
    if let Some(t) = &args.token {
        req.headers_mut()
            .insert("authorization", format!("Bearer {t}").parse()?);
    }
    let access = access_headers(
        args.cf_access_client_id.as_deref(),
        args.cf_access_client_secret.as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    for (k, v) in access {
        req.headers_mut().insert(k, v.parse()?);
    }
    let (ws, _) = connect_async(req)
        .await
        .with_context(|| format!("connecting to {}", args.url))?;
    Ok(ws)
}

async fn send(ws: &mut Ws, m: &ClientMsg) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(m)?.into()))
        .await?;
    Ok(())
}

async fn recv(ws: &mut Ws) -> Result<ServerMsg> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t)?),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => bail!("connection closed"),
        }
    }
}

fn print_event(task: TaskId, event: &AgentEvent) {
    let short = &task.to_string()[..8];
    match event {
        AgentEvent::Planned { harness, prompt } => println!("[{short}] plan: {harness}: {prompt}"),
        AgentEvent::Started { .. } => println!("[{short}] started"),
        AgentEvent::Text { text } => println!("[{short}] {text}"),
        AgentEvent::ToolUse { name, input } => println!("[{short}] > {name} {input}"),
        AgentEvent::ToolResult { content } => {
            let first = content.lines().next().unwrap_or("");
            println!("[{short}] < {first}");
        }
        AgentEvent::Finished { ok, summary } => {
            println!(
                "[{short}] {} {}",
                if *ok { "done" } else { "FAILED" },
                summary.as_deref().unwrap_or("")
            )
        }
    }
}

/// First line of `s`, cut to `max` chars with a trailing ellipsis.
fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max - 1).collect();
    format!("{cut}…")
}

/// The one task whose id starts with `prefix`.
fn resolve(tasks: &[TaskSummary], prefix: &str) -> Result<TaskId> {
    let hits: Vec<_> = tasks
        .iter()
        .filter(|t| t.id.to_string().starts_with(prefix))
        .collect();
    match hits.as_slice() {
        [t] => Ok(t.id),
        [] => bail!("no task matches {prefix}"),
        _ => bail!("{prefix} matches {} tasks", hits.len()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Cmd::Dev { skip_permissions } = args.cmd {
        return dev(&args.url, args.token.as_deref(), skip_permissions).await;
    }
    check_nested(&args.cmd, std::env::var(TASK_ENV).ok().as_deref())?;
    let mut ws = connect(&args).await?;
    match &args.cmd {
        Cmd::Status => {
            send(&mut ws, &ClientMsg::Status).await?;
            let ServerMsg::Status { workers, tasks } = recv(&mut ws).await? else {
                bail!("unexpected reply")
            };
            println!("workers ({}):", workers.len());
            for w in workers {
                println!(
                    "  {} {} {} {:?} roots={}{}",
                    w.id,
                    w.info.hostname,
                    w.info.os,
                    w.info.harnesses,
                    w.info.roots.join(","),
                    if !w.online {
                        " [offline]"
                    } else if w.busy {
                        " [busy]"
                    } else {
                        ""
                    }
                );
            }
            println!("tasks ({}):", tasks.len());
            for t in tasks {
                println!(
                    "  {} {:<8} {}{}",
                    &t.id.to_string()[..8],
                    format!("{:?}", t.status).to_lowercase(),
                    t.prompt,
                    t.summary
                        .map(|s| format!(" -> {}", first_line(&s, 80)))
                        .unwrap_or_default()
                );
            }
        }
        Cmd::Show { id } => {
            send(&mut ws, &ClientMsg::Status).await?;
            let ServerMsg::Status { tasks, .. } = recv(&mut ws).await? else {
                bail!("unexpected reply")
            };
            let task = resolve(&tasks, id)?;
            send(&mut ws, &ClientMsg::Show { task }).await?;
            let (t, events, truncated) = match recv(&mut ws).await? {
                ServerMsg::TaskDetail {
                    task,
                    events,
                    truncated,
                } => (task, events, truncated),
                ServerMsg::Error { message } => bail!(message),
                other => bail!("unexpected reply: {other:?}"),
            };
            let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
            println!("id:      {}", t.id);
            println!("status:  {}", format!("{:?}", t.status).to_lowercase());
            println!("harness: {}", t.harness);
            println!("worker:  {}", opt(t.worker.map(|w| w.to_string())));
            println!("session: {}", opt(t.session));
            println!("prompt:  {}", t.prompt);
            println!("summary: {}", opt(t.summary));
            println!();
            if truncated {
                println!("(earlier events dropped)");
            }
            for e in &events {
                print_event(t.id, e);
            }
        }
        Cmd::Task {
            prompt,
            cwd,
            harness,
            follow,
            nested: _,
        } => {
            if *follow {
                send(&mut ws, &ClientMsg::Subscribe).await?;
            }
            send(
                &mut ws,
                &ClientMsg::Submit {
                    prompt: prompt.clone(),
                    cwd: cwd.clone(),
                    harness: harness.clone(),
                },
            )
            .await?;
            // Other tasks' events may interleave once subscribed; skip them until Submitted.
            let task = loop {
                match recv(&mut ws).await? {
                    ServerMsg::Submitted { task } => break task,
                    ServerMsg::Event { .. } => continue,
                    other => bail!("unexpected reply: {other:?}"),
                }
            };
            println!("submitted {task}");
            if *follow {
                loop {
                    match recv(&mut ws).await? {
                        ServerMsg::Event { task: t, event, .. } if t == task => {
                            print_event(t, &event);
                            if let AgentEvent::Finished { ok, .. } = event {
                                if !ok {
                                    std::process::exit(1);
                                }
                                break;
                            }
                        }
                        ServerMsg::Error { message } => eprintln!("warning: {message}"),
                        _ => {}
                    }
                }
            }
        }
        Cmd::Tail => {
            send(&mut ws, &ClientMsg::Subscribe).await?;
            loop {
                match recv(&mut ws).await? {
                    ServerMsg::Event { task, event, .. } => print_event(task, &event),
                    ServerMsg::Error { message } => eprintln!("warning: {message}"),
                    _ => {}
                }
            }
        }
        Cmd::Dev { .. } => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(nested: bool) -> Cmd {
        Cmd::Task {
            prompt: "p".into(),
            cwd: None,
            harness: None,
            follow: false,
            nested,
        }
    }

    fn summary(id: &str) -> TaskSummary {
        TaskSummary {
            id: id.parse().unwrap(),
            prompt: String::new(),
            status: TaskStatus::Done,
            worker: None,
            harness: "claude".into(),
            session: None,
            summary: None,
        }
    }

    #[test]
    fn nested_submit_refused_unless_opted_in() {
        assert!(check_nested(&task(false), Some("abc")).is_err());
        assert!(check_nested(&task(true), Some("abc")).is_ok());
        assert!(check_nested(&task(false), None).is_ok());
        assert!(check_nested(&task(false), Some("")).is_ok());
        assert!(check_nested(&Cmd::Status, Some("abc")).is_ok());
    }

    #[test]
    fn resolve_needs_one_match() {
        let tasks = [
            summary("aaaa0000-0000-0000-0000-000000000000"),
            summary("aaab0000-0000-0000-0000-000000000000"),
        ];
        assert_eq!(resolve(&tasks, "aaab").unwrap(), tasks[1].id);
        assert!(resolve(&tasks, "aaa").is_err());
        assert!(resolve(&tasks, "b").is_err());
    }

    #[test]
    fn first_line_truncates() {
        assert_eq!(first_line("ab\ncd", 80), "ab");
        assert_eq!(first_line("abcdef", 4), "abc…");
    }
}
