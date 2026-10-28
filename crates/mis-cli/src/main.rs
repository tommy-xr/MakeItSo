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

/// The question to send: `[task-id] [question]`, a lone hex word being an id.
fn ask_question(words: &[String]) -> String {
    let is_id = |w: &str| w.len() >= 4 && w.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    match words {
        [] => FLEET_QUESTION.into(),
        [id] if is_id(id) => format!(
            "About task {id}: what was it asked, what did it do, and how did it end? One paragraph."
        ),
        [q] => q.clone(),
        [id, q, ..] => format!("About task {id}: {q}"),
    }
}

/// `ws://h:1/` -> `http://h:1/mcp`, `wss://h` -> `https://h/mcp`.
fn mcp_url(ws: &str) -> Result<String> {
    let rest = ws
        .strip_prefix("ws")
        .with_context(|| format!("need a ws:// or wss:// url, got {ws}"))?;
    Ok(format!("http{}/mcp", rest.trim_end_matches('/')))
}

/// `claude` argv and env for `mis ask`. Header values ride in env vars
/// (`${MIS_ASK_H0}`, which claude expands) so secrets stay out of `ps`.
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
    let names: serde_json::Map<String, serde_json::Value> = headers
        .iter()
        .zip(&env)
        .map(|((k, _), (var, _))| (k.to_string(), format!("${{{var}}}").into()))
        .collect();
    let server = mis_agent::MCP_SERVER;
    let config =
        serde_json::json!({"mcpServers": {server: {"type": "http", "url": url, "headers": names}}});
    let argv = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        &format!("--model={model}"),
        "--system-prompt",
        ASK_SYSTEM,
        // No user hooks, CLAUDE.md, other MCP servers or built-in tools: only ours.
        "--setting-sources",
        "",
        "--tools",
        "",
        "--mcp-config",
        &config.to_string(),
        "--strict-mcp-config",
        "--allowedTools",
        &format!("mcp__{server}__*"),
        "--max-turns",
        "6",
        "--",
        question,
    ]
    .map(String::from)
    .to_vec();
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
    let (argv, env) = ask_command(&mcp_url(&args.url)?, model, &ask_question(words), &headers);
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
                    bail!("claude failed: {}", summary.unwrap_or_default())
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

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
    if let Cmd::Ask { words, ask_model } = &args.cmd {
        return ask(&args, words, ask_model).await;
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
            for t in tasks {
                println!(
                    "  {} {:<8} {}\n      {}{}",
                    &t.id.to_string()[..8],
                    format!("{:?}", t.status).to_lowercase(),
                    task_line(&t, &workers, now),
                    t.prompt,
                    t.summary
                        .map(|s| format!(" -> {}", first_line(&s, 80)))
                        .unwrap_or_default()
                );
            }
        }
        Cmd::Show { id } => {
            send(&mut ws, &ClientMsg::Status).await?;
            let ServerMsg::Status { tasks, workers, .. } = recv(&mut ws).await? else {
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
            println!("harness: {}", placement(&t, &workers));
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
            model,
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
                    model: model.clone(),
                },
            )
            .await?;
            // Broadcasts may interleave once subscribed; skip them until Submitted.
            let task = loop {
                match recv(&mut ws).await? {
                    ServerMsg::Submitted { task } => break task,
                    ServerMsg::Event { .. } | ServerMsg::Status { .. } => continue,
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
        Cmd::Dev { .. } | Cmd::Ask { .. } => unreachable!(),
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
            model: None,
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
            model: None,
            session: None,
            summary: None,
            started_at: None,
            finished_at: None,
            tokens_in: None,
            tokens_out: None,
            activity: String::new(),
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
    fn ask_question_reads_id_and_question() {
        let w = |xs: &[&str]| ask_question(&xs.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(w(&[]), FLEET_QUESTION);
        assert_eq!(w(&["why so slow?"]), "why so slow?");
        assert!(w(&["81df88c0"]).starts_with("About task 81df88c0: what was it asked"));
        assert_eq!(w(&["81df", "why?"]), "About task 81df: why?");
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
        assert_eq!(at("--allowedTools"), "mcp__mis__*");
        assert_eq!(at("--tools"), "");
        assert_eq!(at("--max-turns"), "6");
        assert!(argv.contains(&"--model=haiku".to_string()));
        // Question last, after `--`, so a leading '-' isn't a flag.
        assert_eq!(argv[argv.len() - 2..], ["--", "-q"]);
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
    fn stats_text_formats_or_dashes() {
        assert_eq!(stats_text(None), "stats: -");
        let s = WorkerStats {
            disk_free_gb: 1.0,
            disk_used_pct: 38,
            cpu_pct: 64,
            mem_pct: 82,
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
