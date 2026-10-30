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
        /// Model for the harness, e.g. `sonnet`; implies the default harness if none given.
        #[arg(long)]
        model: Option<String>,
        /// Stream this task's events until it finishes.
        #[arg(long, short)]
        follow: bool,
        /// Wait quietly for the task to finish, then print the agent's summary. Exit 0 if it succeeded,
        /// 1 if it failed, 2 if there's no result (connection or usage error).
        #[arg(long, conflicts_with = "follow")]
        wait: bool,
        /// With --wait: print the finished task as JSON instead.
        #[arg(long, requires = "wait")]
        json: bool,
        /// Allow submitting from inside a running task (set by MIS_TASK).
        #[arg(long)]
        nested: bool,
    },
    /// Reply in a task's thread: resumes its agent session on the same worker.
    Reply {
        /// Task id or unambiguous prefix.
        id: String,
        text: String,
        /// Return once submitted instead of streaming the reply's events.
        #[arg(long)]
        no_follow: bool,
        /// Wait quietly for the task to finish, then print the agent's summary. Exit 0 if it succeeded,
        /// 1 if it failed, 2 if there's no result (connection or usage error).
        #[arg(long, conflicts_with = "no_follow")]
        wait: bool,
        /// With --wait: print the finished task as JSON instead.
        #[arg(long, requires = "wait")]
        json: bool,
    },
    /// Show one task's details and transcript.
    Show {
        /// Task id or unambiguous prefix (as `mis status` prints).
        id: String,
    },
    /// Stream all task events.
    Tail,
    /// Ask a local model about the fleet; it reads the orchestrator's MCP tools.
    /// `mis ask`, `mis ask "<question>"`, `mis ask <task-id> ["<question>"]`.
    Ask {
        #[arg(num_args = 0..=2, value_names = ["TASK_ID", "QUESTION"])]
        words: Vec<String>,
        /// Model `claude` answers with.
        #[arg(long, env = "MIS_ASK_MODEL", default_value = "haiku")]
        ask_model: String,
    },
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

const FLEET_QUESTION: &str = "Give a one-paragraph status of the fleet: what's running, \
     what finished, anything needing attention.";

const ASK_SYSTEM: &str = "You answer questions about a Make It So fleet: worker machines \
     running coding-agent tasks. Look things up with the mis tools; never guess. Be terse: \
     plain sentences, no headings, no tables. Name tasks by their first 8 id characters.";

/// The question to send: `[task-id] [question]`, a lone hex word of 8+ chars
/// (as `mis status` prints ids) being an id.
fn ask_question(words: &[String]) -> Result<String> {
    let is_id = |w: &str| w.len() >= 8 && w.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    Ok(match words {
        [] => FLEET_QUESTION.into(),
        [id] if is_id(id) => format!(
            "About task {id}: what was it asked, what did it do, and how did it end? One paragraph."
        ),
        [q] => q.clone(),
        [id, q] if is_id(id) => format!("About task {id}: {q}"),
        _ => bail!("usage: mis ask [TASK_ID] [QUESTION]"),
    })
}

/// `ws://h:1/` -> `http://h:1/mcp`, `wss://h` -> `https://h/mcp`.
fn mcp_url(ws: &str) -> Result<String> {
    let rest = ws
        .strip_prefix("ws")
        .with_context(|| format!("need a ws:// or wss:// url, got {ws}"))?;
    Ok(format!("http{}/mcp", rest.trim_end_matches('/')))
}

/// `claude` argv and env for `mis ask`. Header values ride in env vars
/// (`${MIS_ASK_H0}`) so secrets stay out of `ps`.
fn ask_command(
    url: &str,
    model: &str,
    question: &str,
    headers: &[(&str, String)],
) -> (Vec<String>, Vec<(String, String)>) {
    let env: Vec<(String, String)> = headers
        .iter()
        .enumerate()
        .map(|(i, (_, v))| (format!("MIS_ASK_H{i}"), v.clone()))
        .collect();
    let refs: Vec<(&str, String)> = headers
        .iter()
        .zip(&env)
        .map(|((k, _), (var, _))| (*k, format!("${{{var}}}")))
        .collect();
    let mut argv: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        &format!("--model={model}"),
        "--system-prompt",
        ASK_SYSTEM,
        // No user hooks, CLAUDE.md or built-in tools: only our MCP server.
        "--setting-sources",
        "",
        "--tools",
        "",
        "--max-turns",
        "6",
    ]
    .map(String::from)
    .to_vec();
    argv.extend(mis_agent::claude::mcp_args(url, &refs));
    argv.extend(["--".into(), question.into()]);
    (argv, env)
}

/// Run `claude` per `ask_command`, printing its text as it arrives.
async fn ask(args: &Args, words: &[String], model: &str) -> Result<()> {
    use tokio::io::AsyncBufReadExt;
    let mut headers: Vec<(&str, String)> = access_headers(
        args.cf_access_client_id.as_deref(),
        args.cf_access_client_secret.as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    if let Some(t) = &args.token {
        headers.push(("authorization", format!("Bearer {t}")));
    }
    // Same auth as /mcp: a bad token fails here, not as a vague model answer.
    connect(args).await?;
    let question = ask_question(words)?;
    let (argv, env) = ask_command(&mcp_url(&args.url)?, model, &question, &headers);
    let mut child = tokio::process::Command::new("claude")
        .args(argv)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning claude")?;
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    while let Some(line) = lines.next_line().await? {
        for e in mis_agent::claude::parse_line(&line) {
            match e {
                AgentEvent::Text { text } => println!("{}", text.trim()),
                AgentEvent::Finished { ok: false, summary } => {
                    let why = summary.unwrap_or_else(|| "no answer within 6 turns".into());
                    bail!("claude failed: {why}")
                }
                _ => {}
            }
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        bail!("claude exited: {status}");
    }
    Ok(())
}

/// An agent submitting to the fleet that is running it deadlocks on a single
/// worker, so refuse unless `--nested`. The message deliberately omits the flag:
/// agents do what error messages say.
fn check_nested(cmd: &Cmd, task_env: Option<&str>) -> Result<()> {
    match (cmd, task_env) {
        (Cmd::Task { nested: false, .. } | Cmd::Reply { .. }, Some(parent))
            if !parent.is_empty() =>
        {
            bail!(
            "refusing to submit from inside task {parent}: with one worker it can never start while this task runs. Do the work here."
        )
        }
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
        AgentEvent::Planned {
            harness,
            prompt,
            model,
        } => {
            let model = model.as_ref().map(|m| format!("/{m}")).unwrap_or_default();
            println!("[{short}] plan: {harness}{model}: {prompt}")
        }
        AgentEvent::Started { .. } => println!("[{short}] started"),
        AgentEvent::Text { text } => println!("[{short}] {text}"),
        AgentEvent::ToolUse { name, input } => println!("[{short}] > {name} {input}"),
        AgentEvent::ToolResult { content } => {
            let first = content.lines().next().unwrap_or("");
            println!("[{short}] < {first}");
        }
        AgentEvent::Status { text } => println!("[{short}] status: {text}"),
        AgentEvent::Evidence(l) => println!("[{short}] evidence: {}", l.line()),
        AgentEvent::Limited { .. } => println!("[{short}] usage limit hit"),
        // Kept by the worker, never forwarded.
        AgentEvent::Limits { .. } => {}
        // Running totals; `mis status` shows the latest.
        AgentEvent::Usage { .. } => {}
        AgentEvent::Finished { ok, summary } => {
            println!(
                "[{short}] {} {}",
                if *ok { "done" } else { "FAILED" },
                summary.as_deref().unwrap_or("")
            )
        }
    }
}

/// `disk 38% cpu 64% mem 82%`, or `stats: -` before the first report.
fn stats_text(s: Option<&WorkerStats>) -> String {
    s.map_or("stats: -".into(), |s| {
        format!(
            "disk {}% cpu {}% mem {}%",
            s.disk_used_pct, s.cpu_pct, s.mem_pct
        )
    })
}

/// `claude/sonnet @ host`: model and host only when known; `-` before a harness is picked.
fn placement(t: &TaskSummary, workers: &[WorkerSummary]) -> String {
    let mut s = if t.harness.is_empty() {
        "-".to_string()
    } else {
        t.harness.clone()
    };
    if let Some(m) = &t.model {
        s += &format!("/{m}");
    }
    if let Some(wid) = t.worker {
        let host = workers.iter().find(|w| w.id == wid).map_or_else(
            || wid.to_string()[..8].to_string(),
            |w| w.info.hostname.clone(),
        );
        s += &format!(" @ {host}");
    }
    s
}

/// `850`, `12.3k`, `1.2M`.
fn tokens(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// `claude/sonnet @ host · 1m04s · 12.3k/1.1k · > Bash ls`; tokens only once
/// reported. A running task's time runs to `now_ms`.
fn task_line(t: &TaskSummary, workers: &[WorkerSummary], now_ms: u64) -> String {
    let mut parts = vec![placement(t, workers)];
    if let Some(start) = t.started_at {
        parts.push(fmt_elapsed(
            t.finished_at.unwrap_or(now_ms).saturating_sub(start),
        ));
    }
    if let (Some(i), Some(o)) = (t.tokens_in, t.tokens_out) {
        parts.push(format!("{}/{}", tokens(i), tokens(o)));
    }
    parts.push(t.activity.clone());
    parts.join(" · ")
}

/// The id from the reply to a Submit. Broadcasts may interleave once
/// subscribed; skip them until Submitted.
async fn submitted(ws: &mut Ws) -> Result<TaskId> {
    loop {
        match recv(ws).await? {
            ServerMsg::Submitted { task } => return Ok(task),
            ServerMsg::Error { message } => bail!(message),
            ServerMsg::Event { .. } | ServerMsg::Status { .. } => continue,
            other => bail!("unexpected reply: {other:?}"),
        }
    }
}

/// Print `task`'s events until it finishes; exit 1 if it failed.
async fn follow_task(ws: &mut Ws, task: TaskId) -> Result<()> {
    loop {
        match recv(ws).await? {
            ServerMsg::Event { task: t, event, .. } if t == task => {
                print_event(t, &event);
                if let AgentEvent::Finished { ok, .. } = event {
                    if !ok {
                        std::process::exit(1);
                    }
                    return Ok(());
                }
            }
            ServerMsg::Error { message } => eprintln!("warning: {message}"),
            _ => {}
        }
    }
}

/// Where a `--wait` stands after one message from the orchestrator.
#[derive(Debug, PartialEq)]
enum Wait {
    Pending,
    /// Ask for a Status: the task just finished, or its Finished may have been dropped.
    Check,
    Over(Box<TaskSummary>),
}

fn wait_step(task: TaskId, msg: ServerMsg) -> Wait {
    match msg {
        ServerMsg::Event {
            task: t,
            event: AgentEvent::Finished { .. },
            ..
        } if t == task => Wait::Check,
        // e.g. "dropped N events" when this subscriber lagged.
        ServerMsg::Error { .. } => Wait::Check,
        ServerMsg::Status { tasks, .. } => tasks
            .into_iter()
            .find(|t| t.id == task && matches!(t.status, TaskStatus::Done | TaskStatus::Failed))
            .map_or(Wait::Pending, |t| Wait::Over(Box::new(t))),
        _ => Wait::Pending,
    }
}

/// Follow `task` quietly until it finishes; the task as it was then (a summarizer
/// may still replace its summary later).
async fn wait_task(ws: &mut Ws, task: TaskId) -> Result<TaskSummary> {
    loop {
        match wait_step(task, recv(ws).await?) {
            Wait::Pending => {}
            Wait::Check => send(ws, &ClientMsg::Status).await?,
            Wait::Over(t) => return Ok(*t),
        }
    }
}

/// `--wait`'s output: the summary (else the status), or the whole task as JSON.
fn print_final(t: &TaskSummary, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(t)?);
    } else {
        let status = format!("{:?}", t.status).to_lowercase();
        println!("{}", t.summary.as_deref().unwrap_or(&status));
    }
    Ok(())
}

/// 0 ok; 1 a waited-for task failed, or an error; 2 an error under `--wait`, so
/// scripts can tell "the task failed" from "no result".
fn exit_code(waits: bool, result: &Result<Option<TaskSummary>>) -> u8 {
    match result {
        Ok(Some(t)) if t.status != TaskStatus::Done => 1,
        Ok(_) => 0,
        Err(_) if waits => 2,
        Err(_) => 1,
    }
}

/// Tasks in order with each one's replies right after it, nested: (depth, task).
/// A reply whose parent isn't listed shows at the top level.
fn threaded(tasks: &[TaskSummary]) -> Vec<(usize, &TaskSummary)> {
    fn add<'a>(
        out: &mut Vec<(usize, &'a TaskSummary)>,
        tasks: &'a [TaskSummary],
        t: &'a TaskSummary,
        depth: usize,
    ) {
        out.push((depth, t));
        for r in tasks.iter().filter(|r| r.parent == Some(t.id)) {
            add(out, tasks, r, depth + 1);
        }
    }
    let mut out = vec![];
    let listed = |id: TaskId| tasks.iter().any(|t| t.id == id);
    for t in tasks.iter().filter(|t| !t.parent.is_some_and(listed)) {
        add(&mut out, tasks, t, 0);
    }
    out
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let waits = matches!(
        args.cmd,
        Cmd::Task { wait: true, .. } | Cmd::Reply { wait: true, .. }
    );
    let result = run(args).await;
    if let Err(e) = &result {
        eprintln!("Error: {e:?}");
    }
    exit_code(waits, &result).into()
}

/// Runs the command; the finished task when it waited for one.
async fn run(args: Args) -> Result<Option<TaskSummary>> {
    if let Cmd::Dev { skip_permissions } = args.cmd {
        dev(&args.url, args.token.as_deref(), skip_permissions).await?;
        return Ok(None);
    }
    if let Cmd::Ask { words, ask_model } = &args.cmd {
        ask(&args, words, ask_model).await?;
        return Ok(None);
    }
    check_nested(&args.cmd, std::env::var(TASK_ENV).ok().as_deref())?;
    let mut ws = connect(&args).await?;
    match &args.cmd {
        Cmd::Status => {
            send(&mut ws, &ClientMsg::Status).await?;
            let ServerMsg::Status {
                workers,
                tasks,
                now_ms,
                ..
            } = recv(&mut ws).await?
            else {
                bail!("unexpected reply")
            };
            // Orchestrator's clock, like the task timestamps; older ones don't send it.
            let now = now_ms.unwrap_or_else(unix_ms);
            println!("{}", headline(&workers, &tasks, now));
            println!("workers ({}):", workers.len());
            for w in &workers {
                let current = tasks
                    .iter()
                    .find(|t| t.worker == Some(w.id) && t.status == TaskStatus::Running)
                    .map(|t| format!(" · {} {}", &t.id.to_string()[..8], t.activity))
                    .unwrap_or_default();
                println!(
                    "  {} {} {} {:?} roots={} {}{}{current}",
                    w.id,
                    w.info.hostname,
                    w.info.os,
                    w.info.harnesses,
                    w.info.roots.join(","),
                    stats_text(w.stats.as_ref()),
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
            for (depth, t) in threaded(&tasks) {
                let pad = "    ".repeat(depth);
                let mark = if depth > 0 { "↳ " } else { "" };
                println!(
                    "  {pad}{mark}{} {:<8} {}\n      {pad}{}{}",
                    &t.id.to_string()[..8],
                    format!("{:?}", t.status).to_lowercase(),
                    task_line(t, &workers, now),
                    first_line(&t.prompt, 100),
                    t.summary
                        .as_ref()
                        .map(|s| format!(" -> {}", first_line(s, 80)))
                        .unwrap_or_default()
                );
            }
        }
        Cmd::Show { id } => {
            send(&mut ws, &ClientMsg::Status).await?;
            let ServerMsg::Status { tasks, workers, .. } = recv(&mut ws).await? else {
                bail!("unexpected reply")
            };
            let task = resolve_task(&tasks, id).map_err(anyhow::Error::msg)?;
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
            println!("harness: {}", placement(&t, &workers));
            println!("worker:  {}", opt(t.worker.map(|w| w.to_string())));
            println!("session: {}", opt(t.session));
            println!("parent:  {}", opt(t.parent.map(|p| p.to_string())));
            let replies: Vec<String> = tasks
                .iter()
                .filter(|r| r.parent == Some(t.id))
                .map(|r| format!("{} {}", &r.id.to_string()[..8], first_line(&r.prompt, 60)))
                .collect();
            if !replies.is_empty() {
                println!("replies: {}", replies.join("\n         "));
            }
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
            model,
            follow,
            wait,
            json,
            nested: _,
        } => {
            if *follow || *wait {
                send(&mut ws, &ClientMsg::Subscribe).await?;
            }
            send(
                &mut ws,
                &ClientMsg::Submit {
                    prompt: prompt.clone(),
                    cwd: cwd.clone(),
                    harness: harness.clone(),
                    model: model.clone(),
                    reply_to: None,
                    images: vec![],
                },
            )
            .await?;
            let task = submitted(&mut ws).await?;
            if *wait {
                eprintln!("submitted {task}");
                let t = wait_task(&mut ws, task).await?;
                print_final(&t, *json)?;
                return Ok(Some(t));
            }
            println!("submitted {task}");
            if *follow {
                follow_task(&mut ws, task).await?;
            }
        }
        Cmd::Reply {
            id,
            text,
            no_follow,
            wait,
            json,
        } => {
            send(&mut ws, &ClientMsg::Status).await?;
            let ServerMsg::Status { tasks, .. } = recv(&mut ws).await? else {
                bail!("unexpected reply")
            };
            let parent = resolve_task(&tasks, id).map_err(anyhow::Error::msg)?;
            if !no_follow {
                send(&mut ws, &ClientMsg::Subscribe).await?;
            }
            send(
                &mut ws,
                &ClientMsg::Submit {
                    prompt: text.clone(),
                    cwd: None,
                    harness: None,
                    model: None,
                    reply_to: Some(parent),
                    images: vec![],
                },
            )
            .await?;
            let task = submitted(&mut ws).await?;
            let line = format!("submitted {task} (reply to {})", &parent.to_string()[..8]);
            if *wait {
                eprintln!("{line}");
                let t = wait_task(&mut ws, task).await?;
                print_final(&t, *json)?;
                return Ok(Some(t));
            }
            println!("{line}");
            if !no_follow {
                follow_task(&mut ws, task).await?;
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
        Cmd::Dev { .. } | Cmd::Ask { .. } => unreachable!(),
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(nested: bool) -> Cmd {
        Cmd::Task {
            prompt: "p".into(),
            cwd: None,
            harness: None,
            model: None,
            follow: false,
            wait: false,
            json: false,
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
            model: None,
            session: None,
            summary: None,
            started_at: None,
            finished_at: None,
            tokens_in: None,
            tokens_out: None,
            activity: String::new(),
            parent: None,
            links: vec![],
            attention: Attention::None,
            blocked_until: None,
            images: vec![],
        }
    }

    #[test]
    fn nested_submit_refused_unless_opted_in() {
        assert!(check_nested(&task(false), Some("abc")).is_err());
        assert!(check_nested(&task(true), Some("abc")).is_ok());
        assert!(check_nested(&task(false), None).is_ok());
        assert!(check_nested(&task(false), Some("")).is_ok());
        assert!(check_nested(&Cmd::Status, Some("abc")).is_ok());
        let reply = Cmd::Reply {
            id: "a".into(),
            text: "t".into(),
            no_follow: false,
            wait: false,
            json: false,
        };
        assert!(check_nested(&reply, Some("abc")).is_err());
    }

    #[test]
    fn wait_checks_on_finish_or_error_and_ends_on_a_final_status() {
        let id: TaskId = "00000000-0000-0000-0000-00000000000a".parse().unwrap();
        let other: TaskId = "00000000-0000-0000-0000-00000000000b".parse().unwrap();
        let finished = |task| ServerMsg::Event {
            task,
            worker: None,
            event: AgentEvent::Finished {
                ok: true,
                summary: None,
            },
        };
        let status = |s| {
            let mut t = summary(&id.to_string());
            t.status = s;
            ServerMsg::Status {
                workers: vec![],
                tasks: vec![summary(&other.to_string()), t],
                now_ms: None,
                update: Update::None,
            }
        };
        assert_eq!(wait_step(id, finished(other)), Wait::Pending);
        assert_eq!(wait_step(id, finished(id)), Wait::Check);
        let lagged = ServerMsg::Error {
            message: "dropped 3 events; re-run status".into(),
        };
        assert_eq!(wait_step(id, lagged), Wait::Check);
        let text = ServerMsg::Event {
            task: id,
            worker: None,
            event: AgentEvent::Text { text: "hi".into() },
        };
        assert_eq!(wait_step(id, text), Wait::Pending);
        assert_eq!(wait_step(id, status(TaskStatus::Running)), Wait::Pending);
        for s in [TaskStatus::Done, TaskStatus::Failed] {
            let Wait::Over(t) = wait_step(id, status(s)) else {
                panic!("{s:?}")
            };
            assert_eq!((t.id, t.status), (id, s));
        }
    }

    #[test]
    fn exit_codes_tell_failed_from_no_result() {
        let done = Ok(Some(summary("00000000-0000-0000-0000-00000000000a")));
        let mut t = summary("00000000-0000-0000-0000-00000000000a");
        t.status = TaskStatus::Failed;
        let failed = Ok(Some(t));
        let err = || Err(anyhow::anyhow!("connecting"));
        assert_eq!(exit_code(true, &done), 0);
        assert_eq!(exit_code(true, &failed), 1);
        assert_eq!(exit_code(true, &err()), 2);
        assert_eq!(exit_code(false, &Ok(None)), 0);
        assert_eq!(exit_code(false, &err()), 1);
    }

    #[test]
    fn json_needs_wait_and_wait_excludes_follow() {
        let parse = |xs: &[&str]| Args::try_parse_from([&["mis"], xs].concat());
        assert!(parse(&["task", "p", "--wait", "--json"]).is_ok());
        assert!(parse(&["reply", "ab", "t", "--wait"]).is_ok());
        assert!(parse(&["task", "p", "--json"]).is_err());
        assert!(parse(&["task", "p", "--wait", "--follow"]).is_err());
        assert!(parse(&["reply", "ab", "t", "--wait", "--no-follow"]).is_err());
    }

    #[test]
    fn ask_question_reads_id_and_question() {
        let w = |xs: &[&str]| ask_question(&xs.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(w(&[]).unwrap(), FLEET_QUESTION);
        assert_eq!(w(&["why so slow?"]).unwrap(), "why so slow?");
        assert_eq!(
            w(&["facade"]).unwrap(),
            "facade",
            "short hex word is a question"
        );
        assert!(w(&["81df88c0"])
            .unwrap()
            .starts_with("About task 81df88c0: what was it"));
        assert_eq!(
            w(&["81df88c0", "why?"]).unwrap(),
            "About task 81df88c0: why?"
        );
        assert!(w(&["why slow", "x"]).is_err());
    }

    #[test]
    fn ask_command_points_claude_at_mcp_only() {
        assert_eq!(
            mcp_url("ws://127.0.0.1:7474/").unwrap(),
            "http://127.0.0.1:7474/mcp"
        );
        assert_eq!(mcp_url("wss://m.example").unwrap(), "https://m.example/mcp");
        assert!(mcp_url("http://x").is_err());
        let headers = [("authorization", "Bearer s3cret".to_string())];
        let (argv, env) = ask_command("http://h/mcp", "haiku", "-q", &headers);
        assert!(
            !argv.iter().any(|a| a.contains("s3cret")),
            "secret not in argv"
        );
        assert_eq!(
            env,
            [("MIS_ASK_H0".to_string(), "Bearer s3cret".to_string())]
        );
        let at = |flag: &str| &argv[argv.iter().position(|a| a == flag).unwrap() + 1];
        let config: serde_json::Value = serde_json::from_str(at("--mcp-config")).unwrap();
        let mis = &config["mcpServers"]["mis"];
        assert_eq!(mis["url"], "http://h/mcp");
        assert_eq!(mis["headers"]["authorization"], "${MIS_ASK_H0}");
        assert_eq!(at("--allowedTools"), "mcp__mis");
        assert_eq!(at("--tools"), "");
        assert_eq!(at("--max-turns"), "6");
        assert!(argv.contains(&"--model=haiku".to_string()));
        // Question last, after `--`, so a leading '-' isn't a flag.
        assert_eq!(argv[argv.len() - 2..], ["--", "-q"]);
    }

    #[test]
    fn replies_nest_under_their_parent() {
        let id = |n: u8| format!("{n:08x}-0000-0000-0000-000000000000");
        let mut tasks: Vec<_> = (1..=4).map(|n| summary(&id(n))).collect();
        tasks[2].parent = Some(tasks[0].id); // 3 replies to 1
        tasks[3].parent = Some(tasks[2].id); // 4 replies to 3
        let order: Vec<_> = threaded(&tasks)
            .iter()
            .map(|(d, t)| (*d, t.id.to_string()[..8].to_string()))
            .collect();
        assert_eq!(
            order,
            [(0, id(1)), (1, id(3)), (2, id(4)), (0, id(2))].map(|(d, s)| (d, s[..8].to_string()))
        );
        // Parent not listed: shown at the top.
        assert_eq!(
            threaded(&tasks[2..])
                .iter()
                .map(|(d, _)| *d)
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn stats_text_formats_or_dashes() {
        assert_eq!(stats_text(None), "stats: -");
        let s = WorkerStats {
            disk_free_gb: 1.0,
            disk_used_pct: 38,
            cpu_pct: 64,
            mem_pct: 82,
            ..Default::default()
        };
        assert_eq!(stats_text(Some(&s)), "disk 38% cpu 64% mem 82%");
    }

    #[test]
    fn task_line_shows_time_tokens_activity() {
        let mut t = summary("aaaa0000-0000-0000-0000-000000000000");
        t.activity = "> Bash ls".into();
        assert_eq!(task_line(&t, &[], 0), "claude · > Bash ls");
        t.started_at = Some(1_000);
        t.tokens_in = Some(12_345);
        t.tokens_out = Some(1_100);
        // Running: up to now. Finished: up to finished_at.
        assert_eq!(
            task_line(&t, &[], 65_000),
            "claude · 1m04s · 12.3k/1.1k · > Bash ls"
        );
        t.finished_at = Some(43_000);
        assert_eq!(
            task_line(&t, &[], 9_999_999),
            "claude · 42s · 12.3k/1.1k · > Bash ls"
        );
        assert_eq!(
            (tokens(850), tokens(1_234_567)),
            ("850".into(), "1.2M".into())
        );
    }

    #[test]
    fn placement_shows_known_parts() {
        let mut t = summary("aaaa0000-0000-0000-0000-000000000000");
        assert_eq!(placement(&t, &[]), "claude");
        t.model = Some("sonnet".into());
        let wid: WorkerId = "bbbbbbbb-0000-0000-0000-000000000000".parse().unwrap();
        t.worker = Some(wid);
        assert_eq!(placement(&t, &[]), "claude/sonnet @ bbbbbbbb");
        let w = WorkerSummary {
            id: wid,
            info: WorkerInfo {
                hostname: "mini".into(),
                os: "macos".into(),
                arch: "aarch64".into(),
                harnesses: vec![],
                roots: vec![],
                models: Default::default(),
            },
            busy: false,
            online: true,
            stats: None,
            stats_at: None,
        };
        assert_eq!(placement(&t, &[w]), "claude/sonnet @ mini");
        t.harness.clear();
        t.model = None;
        t.worker = None;
        assert_eq!(placement(&t, &[]), "-");
    }
}
