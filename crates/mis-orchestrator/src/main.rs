use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use mis_orchestrator::access_gate::AccessGate;
use mis_orchestrator::capture::Inbox;
use mis_orchestrator::config::{self, nonempty, Args, Level, UpdateChannel};
use mis_orchestrator::evidence;
use mis_orchestrator::images::Images;
use mis_orchestrator::push::Push;
use mis_orchestrator::transfer;
use mis_orchestrator::{access, normalize_origins, App};

#[derive(Parser)]
#[command(
    about = "Make It So orchestrator",
    version = mis_orchestrator::VERSION,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(flatten)]
    args: Args,
    /// None serves.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Check this machine can host the orchestrator as its env vars (and --env-file) set it
    /// up: sign-ins, notes remote, state dir, power and login. Exit 1 on a failure.
    Doctor {
        /// As the orchestrator's.
        #[arg(long = "env-file", env = "MIS_ENV_FILE")]
        _env_file: Option<std::path::PathBuf>,
        /// Print the checks as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run this orchestrator as a service (a macOS LaunchAgent): started at login, restarted
    /// whenever it exits.
    Service {
        /// LaunchAgent label (testing).
        #[arg(long, hide = true, default_value = mis_orchestrator::service::LABEL)]
        label: String,
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Write this install (state dir, routines, env file) to new archive <FILE>, for `import`
    /// on another machine. Refused while the orchestrator runs.
    Export {
        file: PathBuf,
        /// As the orchestrator's; it goes in the archive. Default: ~/.config/mis/orchestrator.env.
        #[arg(long = "env-file", env = "MIS_ENV_FILE")]
        env_file: Option<PathBuf>,
    },
    /// Unpack an `export` archive into this machine's new install, then run doctor (exit 1 if
    /// it finds a failure).
    Import {
        file: PathBuf,
        /// Where the archive's env file goes. Default: ~/.config/mis/orchestrator.env.
        #[arg(long = "env-file", env = "MIS_ENV_FILE")]
        env_file: Option<PathBuf>,
    },
}

#[derive(clap::Subcommand)]
enum ServiceCmd {
    /// Check it would start (doctor, run as the service will be), then install and start it.
    /// Pins this executable and the state dir, routines dir and env file in use now.
    Install {
        /// As the orchestrator's. Default: ~/.config/mis/orchestrator.env, if there.
        #[arg(long = "env-file", env = "MIS_ENV_FILE")]
        env_file: Option<std::path::PathBuf>,
    },
    /// Stop and remove it; its log stays.
    Uninstall,
    /// Loaded or not, running or not, its last exit and its log's tail.
    Status,
}

/// The channel to publish to workers; see `--update-channel`.
fn channel(arg: Option<UpdateChannel>, ci_build: bool) -> Option<mis_types::Channel> {
    match arg {
        Some(UpdateChannel::Stable) => Some(mis_types::Channel::Stable),
        Some(UpdateChannel::Edge) => Some(mis_types::Channel::Edge),
        Some(UpdateChannel::Off) => None,
        None => ci_build.then_some(mis_types::Channel::Stable),
    }
}

/// Env var `k` as a path, if set and not empty.
fn var(k: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(k)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// `$XDG_STATE_HOME/mis`, else `~/.local/state/mis`; `%LOCALAPPDATA%\mis` on Windows.
fn default_state_dir() -> Option<std::path::PathBuf> {
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")?
    } else {
        var("XDG_STATE_HOME").or_else(|| Some(var("HOME")?.join(".local/state")))?
    };
    Some(base.join("mis"))
}

/// `~/.config/mis`, beside the worker's `worker.toml` (home is `%USERPROFILE%` on Windows).
fn config_dir() -> Option<PathBuf> {
    let home = var("HOME").or_else(|| var("USERPROFILE"))?;
    Some(home.join(".config").join("mis"))
}

/// `~/.config/mis/routines`.
fn default_routines_dir() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("routines"))
}

/// `~/.config/mis/orchestrator.env`, the env file `mis-up` reads.
fn default_env_file() -> Option<PathBuf> {
    Some(config_dir()?.join("orchestrator.env"))
}

/// Local `claude` for one tool-less turn on `model`: no user/project settings
/// (hooks, CLAUDE.md), MCP servers, or tools. (`--bare` would also drop the login.)
fn one_shot_claude(model: &str, extra: &[&str]) -> std::sync::Arc<mis_agent::claude::ClaudeAgent> {
    let base = [
        &format!("--model={model}"),
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--tools",
        "",
        "--max-turns",
        "1",
    ];
    std::sync::Arc::new(mis_agent::claude::ClaudeAgent {
        extra_args: base.iter().chain(extra).map(|s| s.to_string()).collect(),
    })
}

fn certs_url(args: &Args, team: &str) -> String {
    nonempty(&args.cf_access_certs_url).unwrap_or_else(|| access::certs_url(team))
}

/// `--apps-cf-access-aud`: a gate of its own, so a JWT for one Access app never opens the
/// other. Needs the MIS app's team and a public URL (`config::validate`).
async fn apps_access_gate(args: &Args) -> Option<AccessGate> {
    let (Some(aud), Some(team)) = (
        nonempty(&args.apps_cf_access_aud),
        nonempty(&args.cf_access_team),
    ) else {
        return None;
    };
    let gate = AccessGate::remote(team.clone(), aud, certs_url(args, &team));
    if let Err(e) = gate.refresh().await {
        tracing::warn!("fetching Cloudflare Access keys (apps): {e:#}; retrying on first request");
    }
    tracing::info!("Cloudflare Access: apps need the home Access app's JWT");
    Some(gate)
}

/// `--cf-access-team` / `--cf-access-aud` (both or neither: `config::validate`).
async fn access_gate(args: &Args) -> Option<AccessGate> {
    let (Some(team), Some(aud)) = (
        nonempty(&args.cf_access_team),
        nonempty(&args.cf_access_aud),
    ) else {
        return None;
    };
    let url = certs_url(args, &team);
    tracing::info!(%team, "Cloudflare Access: non-local requests need its JWT");
    let mut gate = AccessGate::remote(team, aud, url);
    if let Some(aud) = nonempty(&args.cf_access_capture_aud) {
        tracing::info!("Cloudflare Access: capture app's JWT accepted on /api/capture");
        gate = gate.with_capture_aud(aud);
    }
    // Unreachable now is not fatal: the first request with a JWT refetches.
    if let Err(e) = gate.refresh().await {
        tracing::warn!("fetching Cloudflare Access keys: {e:#}; retrying on first request");
    }
    Some(gate)
}

/// Print ready-to-paste worker/CLI commands for every way to reach this orchestrator.
fn print_connect_hints(
    addr: std::net::SocketAddr,
    token: Option<&str>,
    public_url: Option<&str>,
    origins: &[String],
) {
    let mut urls = Vec::new();
    if let Some(u) = public_url {
        urls.push(u.trim_end_matches('/').to_string());
    }
    if addr.ip().is_loopback() {
        urls.push(format!("ws://{addr}"));
    } else {
        let lan = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !i.is_loopback() && i.ip().is_ipv4())
            .filter(|i| addr.ip().is_unspecified() || i.ip() == addr.ip());
        urls.extend(lan.map(|i| format!("ws://{}:{}", i.ip(), addr.port())));
    }
    let token = token_env(token, std::io::IsTerminal::is_terminal(&std::io::stderr()));
    eprintln!("\nmis-orchestrator {}", mis_orchestrator::VERSION);
    eprintln!("Connect workers with one of (same MIS_URL/MIS_TOKEN work for `mis`):");
    for u in &urls {
        eprintln!("  MIS_URL={}{token} mis-worker", sh_quote(u));
    }
    if let Some(u) = urls.first() {
        eprintln!("Open in a browser: {}/", mis_types::http_url(u));
    }
    if !origins.is_empty() {
        eprintln!("Browser origins allowed: {}", origins.join(", "));
    }
    eprintln!();
}

/// ` MIS_TOKEN=…` for a connect hint: the token only on a terminal, not into a log file.
fn token_env(token: Option<&str>, tty: bool) -> String {
    match token {
        Some(t) if tty => format!(" MIS_TOKEN={}", sh_quote(t)),
        Some(_) => " MIS_TOKEN=<redacted>".into(),
        None => String::new(),
    }
}

/// Single-quote for POSIX shells so tokens like `a$b` or `x y` paste intact.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `--env-file`'s value in `argv` as clap will read it (`--env-file F` or `--env-file=F`).
fn env_file_arg(argv: &[std::ffi::OsString]) -> Option<std::path::PathBuf> {
    let mut args = argv.iter().skip(1);
    while let Some(a) = args.next() {
        if a == "--" {
            break;
        }
        if a == "--env-file" {
            return args.next().map(Into::into);
        }
        if let Some(v) = a.to_str().and_then(|a| a.strip_prefix("--env-file=")) {
            return Some(v.into());
        }
    }
    None
}

/// Set the `--env-file` / `MIS_ENV_FILE` variables not already set, for clap to read. None
/// given: `default`'s, if there.
fn load_env_file(default: Option<PathBuf>) -> anyhow::Result<()> {
    let argv: Vec<_> = std::env::args_os().collect();
    let given = env_file_arg(&argv).or_else(|| var("MIS_ENV_FILE"));
    let Some(path) = given.or_else(|| default.filter(|p| p.is_file())) else {
        return Ok(());
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let vars = mis_types::env_file::parse(&text)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    // The process env wins; a key repeated in the file, its last value.
    let unset: Vec<_> = (vars.into_iter())
        .filter(|(k, _)| std::env::var_os(k).is_none())
        .collect();
    for (k, v) in unset {
        std::env::set_var(k, v);
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // First: no other thread may read the env while it is set. `import` writes the env file;
    // `export` archives the default one and `service install` pins it, so both find the install
    // that file sets up. Not `service uninstall`/`status`: a broken file must not block them.
    let argv: Vec<_> = std::env::args_os().collect();
    let service_install = || argv.iter().skip(2).any(|a| a == "install");
    match argv.get(1) {
        Some(a) if a == "import" => {}
        Some(a) if a == "export" || (a == "service" && service_install()) => {
            load_env_file(default_env_file())?
        }
        _ => load_env_file(None)?,
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let Cli { args, cmd } = Cli::parse();
    let state_dir = args.state_dir.clone().or_else(default_state_dir);
    match cmd {
        Some(Cmd::Doctor { json, .. }) => doctor(&args, state_dir.as_deref(), json),
        Some(Cmd::Service { label, cmd }) => service(&label, cmd, &args, state_dir),
        Some(Cmd::Export { file, env_file }) => {
            export(&file, &install(&args, state_dir, env_file)?)
        }
        Some(Cmd::Import { file, env_file }) => {
            import(&file, &install(&args, state_dir, env_file)?)
        }
        None => tokio::runtime::Runtime::new()?.block_on(serve(args, state_dir)),
    }
}

fn doctor(args: &Args, state_dir: Option<&std::path::Path>, json: bool) -> anyhow::Result<()> {
    use mis_orchestrator::doctor;
    let checks = doctor::checks(args, state_dir, &doctor::probe(args, state_dir));
    match json {
        true => println!("{}", serde_json::to_string_pretty(&checks)?),
        false => print!("{}", doctor::render(&checks)),
    }
    if checks.iter().any(|c| c.level == Level::Fail) {
        std::process::exit(1);
    }
    Ok(())
}

fn service(
    label: &str,
    cmd: ServiceCmd,
    args: &Args,
    state_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    use mis_orchestrator::service;
    match cmd {
        ServiceCmd::Install { env_file } => {
            let no_home = || anyhow::anyhow!("no home dir: set MIS_STATE_DIR and MIS_ROUTINES_DIR");
            let dirs = service::Dirs {
                state_dir: state_dir.ok_or_else(no_home)?,
                routines_dir: (args.routines_dir.clone())
                    .or_else(default_routines_dir)
                    .ok_or_else(no_home)?,
                env_file: env_file.or_else(|| default_env_file().filter(|p| p.is_file())),
            };
            service::install(label, &std::env::current_exe()?.canonicalize()?, dirs)
        }
        ServiceCmd::Uninstall => service::uninstall(label),
        ServiceCmd::Status => service::status(label),
    }
}

/// This install's paths, as serve would use them; `env_file` as given, else the default.
fn install(
    args: &Args,
    state: Option<PathBuf>,
    env_file: Option<PathBuf>,
) -> anyhow::Result<transfer::Paths> {
    let no_home =
        || anyhow::anyhow!("no home dir: set MIS_STATE_DIR, MIS_ROUTINES_DIR and --env-file");
    Ok(transfer::Paths {
        state: state.ok_or_else(no_home)?,
        routines: (args.routines_dir.clone().or_else(default_routines_dir)).ok_or_else(no_home)?,
        env: env_file.or_else(default_env_file).ok_or_else(no_home)?,
    })
}

fn export(file: &Path, from: &transfer::Paths) -> anyhow::Result<()> {
    let e = transfer::export(file, from)?;
    println!("Exported {} files to {}", e.files, file.display());
    println!("  state dir: {}", from.state.display());
    println!("  routines:  {}", or_none(e.routines, &from.routines));
    println!("  env file:  {}", or_none(e.env, &from.env));
    // The tunnel's config and credentials stay put: the new machine may not run it.
    let dir = var("HOME").map(|h| h.join(".cloudflared"));
    let mut files: Vec<_> = (dir.iter().flat_map(std::fs::read_dir).flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    println!(
        "\nNot in the archive: the Cloudflare tunnel's files. To run it on the new machine, copy:"
    );
    match (files.is_empty(), &dir) {
        (false, _) => files.iter().for_each(|f| println!("  {}", f.display())),
        (true, Some(d)) => println!("  (none in {})", d.display()),
        (true, None) => println!("  ~/.cloudflared"),
    }
    println!(
        "Never run the tunnel on both machines at once: stop it here before starting it there."
    );
    Ok(())
}

/// `p`, or none if `had` is false.
fn or_none(had: bool, p: &Path) -> String {
    match had {
        true => p.display().to_string(),
        false => "none".into(),
    }
}

fn import(file: &Path, to: &transfer::Paths) -> anyhow::Result<()> {
    let i = transfer::import(file, to)?;
    println!(
        "Imported {} tasks, {} files from {} (mis-orchestrator {})",
        i.tasks,
        i.files,
        file.display(),
        i.manifest.version
    );
    println!("  state dir: {}", to.state.display());
    println!("  routines:  {}", or_none(i.routines, &to.routines));
    println!("  env file:  {}", or_none(i.env, &to.env));
    match &i.warnings {
        Ok(w) => {
            (w.iter()).for_each(|w| println!("warning: {w}; change it in {}", to.env.display()))
        }
        Err(e) => println!("warning: can't check {}'s paths: {e}", to.env.display()),
    }
    println!("\nNext:");
    println!("  - Run a worker on this machine: app tasks go to the orchestrator's host first.");
    println!("  - On the old machine, point worker.toml's url here, or stop its worker.");
    println!("\nmis-orchestrator doctor:");
    // Of what was placed, whatever the env file says.
    let mut doctor = std::process::Command::new(std::env::current_exe()?);
    doctor.arg("doctor").env_remove("MIS_ENV_FILE");
    doctor.env("MIS_STATE_DIR", &to.state);
    doctor.env("MIS_ROUTINES_DIR", &to.routines);
    if i.env {
        doctor.arg("--env-file").arg(&to.env);
    }
    if !doctor.status().context("running doctor")?.success() {
        std::process::exit(1);
    }
    Ok(())
}

async fn serve(mut args: Args, state_dir: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    // Before `validate`: the updater passes no serve flags, so the config seen here isn't the one served.
    if args.self_test {
        mis_orchestrator::self_test(state_dir.as_deref())?;
        println!("self-test ok");
        return Ok(());
    }
    let findings = config::validate(&args, state_dir.as_deref());
    for f in findings.iter().filter(|f| f.level == Level::Warn) {
        tracing::warn!("{}", f.msg);
    }
    if let Some(f) = findings.iter().find(|f| f.level == Level::Fail) {
        anyhow::bail!("{}", f.msg);
    }
    args.token = nonempty(&args.token); // `MIS_TOKEN=` means no token
    args.read_tokens = config::trimmed(&args.read_tokens);
    args.capture_tokens = config::trimmed(&args.capture_tokens);
    args.access_subjects = config::trimmed(&args.access_subjects);
    let gate = access_gate(&args).await;
    let apps_gate = apps_access_gate(&args).await;
    let apps_listener = match nonempty(&args.apps_bind) {
        Some(bind) => Some(tokio::net::TcpListener::bind(&bind).await?),
        None => None,
    };
    let apps_addr = apps_listener.as_ref().map(|l| l.local_addr()).transpose()?;
    if let Some(addr) = apps_addr {
        tracing::info!("serving apps on http://{addr}/a/");
        if apps_gate.is_none() {
            tracing::info!("no --apps-cf-access-aud: apps answer direct loopback requests only");
        }
        if args.apps_dev {
            tracing::warn!("--apps-dev: direct loopback requests may act as a viewer or editor");
        }
        if !addr.ip().is_loopback() && nonempty(&args.apps_public_url).is_none() {
            tracing::warn!("no --apps-public-url: app links are left out (only loopback names and its host are answered)");
        }
    }
    let apps_public_host = nonempty(&args.apps_public_url)
        .map(|u| mis_orchestrator::app_access::public_authority(&u))
        .transpose()
        .map_err(|e| anyhow::anyhow!("--apps-public-url {e}"))?;
    let apps = state_dir.as_ref().map(|dir| {
        let base =
            mis_orchestrator::apps::base_url(nonempty(&args.apps_public_url).as_deref(), apps_addr);
        std::sync::Arc::new(mis_orchestrator::apps::Apps::new(dir.join("apps"), base))
    });
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    let local = listener.local_addr()?;
    tracing::info!("listening on {local}");
    let origins = normalize_origins(args.allowed_origins);
    let ui_origins =
        mis_orchestrator::apps_http::frame_ancestors(args.public_url.as_deref(), &origins, local);
    print_connect_hints(
        local,
        args.token.as_deref(),
        args.public_url.as_deref(),
        &origins,
    );
    let channel = channel(args.update_channel, mis_update::CI_BUILD);
    // Resolved before any swap: on Linux a replaced executable's path reads "(deleted)".
    let exe = std::env::current_exe()?.canonicalize()?;
    mis_update::cleanup(&exe);
    let source = mis_update::Source::from_env()?;
    let blocked = mis_update::blocked(
        channel.is_none(),
        mis_orchestrator::VERSION,
        mis_update::CI_BUILD,
        source.custom,
    );
    let updater = match (channel, blocked) {
        (Some(channel), None) => {
            tracing::info!(?channel, "self-update on ({})", source.base);
            Some(mis_orchestrator::self_update::Updater {
                channel,
                source,
                exe: exe.clone(),
                state_dir: state_dir.clone(),
                every: std::time::Duration::from_secs(args.update_every),
            })
        }
        (_, why) => {
            tracing::info!(?channel, "self-update off: {}", why.unwrap_or("dev build"));
            None
        }
    };
    let mut app = App::new(args.token)
        .with_host(hostname())
        .with_channel(channel)
        .with_read_tokens(args.read_tokens)
        .with_access_subjects(args.access_subjects)
        .with_capture_tokens(args.capture_tokens)
        .with_allowed_origins(origins);
    if let Some(gate) = gate {
        app = app.with_access(gate);
    }
    if let Some(apps) = &apps {
        app = app.with_apps(apps.clone());
        tokio::spawn(mis_orchestrator::backups::run_daily(apps.clone()));
    }
    if let (Some(listener), Some(apps)) = (apps_listener, apps) {
        let inbox = app.feedback_inbox();
        tokio::spawn(async move {
            let auth = mis_orchestrator::apps_http::Auth {
                gate: apps_gate,
                public_host: apps_public_host,
                dev: args.apps_dev,
                feedback: Some(inbox),
                frame_ancestors: ui_origins,
            };
            if let Err(e) = mis_orchestrator::serve_apps(listener, apps, auth).await {
                tracing::error!("apps listener stopped: {e:#}");
            }
        });
    }
    match args.routines_dir.clone().or_else(default_routines_dir) {
        Some(dir) => app = app.with_routines(dir, mis_orchestrator::ROUTINE_EVERY),
        None => tracing::warn!("no --routines-dir and no home dir: routines off"),
    }
    match state_dir {
        Some(dir) => {
            app = app.with_ledger_dir(dir.clone());
            match Push::open(dir.clone(), args.vapid_subject.clone()) {
                Ok(push) => {
                    tracing::info!(dir = %dir.display(), "push notifications on");
                    app = app.with_push(push);
                }
                Err(e) => tracing::warn!("push notifications off: {e:#}"),
            }
            match Images::open(dir.join("images")) {
                Ok(images) => app = app.with_images(images),
                Err(e) => tracing::warn!("image attachments off: {e:#}"),
            }
            match mis_orchestrator::boards::Boards::open(dir.join("boards")) {
                Ok(boards) => app = app.with_boards(boards),
                Err(e) => tracing::warn!("boards off: {e:#}"),
            }
            match evidence::Store::open(dir.join("evidence")) {
                Ok(store) => app = app.with_evidence(store),
                Err(e) => tracing::warn!("evidence files off: {e:#}"),
            }
            match Inbox::open(dir.join("captures").join("inbox")) {
                Ok(inbox) => app = app.with_captures(inbox),
                Err(e) => tracing::warn!("captures off: {e:#}"),
            }
            if let Some(remote) = nonempty(&args.notes_remote) {
                // Not the URL: it may carry a token.
                tracing::info!("publishing captures to the notes repo");
                app = app.with_notes(mis_orchestrator::notes::Notes::new(remote, &dir));
            }
        }
        None => tracing::warn!(
            "no --state-dir and no home dir: task history is lost on restart; \
             push notifications, image attachments, evidence files and captures off"
        ),
    }
    if args.no_planner {
        tracing::info!("planner off; tasks without --harness run on the default harness");
    } else if mis_agent::which("claude") {
        tracing::info!(model = %args.planner_model, "planner: local claude (tool-less)");
        app = app.with_planner(one_shot_claude(
            &args.planner_model,
            &["--json-schema", mis_orchestrator::plan::SCHEMA],
        ));
        app = app.with_drafter(one_shot_claude(&args.planner_model, &[]));
    } else {
        tracing::warn!(
            "no `claude` on PATH: planner off; tasks without --harness run on the default harness"
        );
    }
    if !args.no_summary && mis_agent::which("claude") {
        tracing::info!(model = %args.summary_model, "summaries: local claude (tool-less)");
        app = app.with_summarizer(one_shot_claude(&args.summary_model, &[]));
        if nonempty(&args.notes_remote).is_some() {
            tracing::info!("inbox suggestions: on, the same model");
            app = app.with_suggester(one_shot_claude(&args.summary_model, &[]));
        }
    }
    // Shut down on a signal, or once the updater has swapped in a new build: then restart on it.
    let restarting = std::cell::Cell::new(false);
    let watched = app.clone();
    let updated = async move {
        match updater {
            Some(u) => u.run(watched).await,
            None => std::future::pending().await,
        }
    };
    let stop = async {
        tokio::select! {
            _ = shutdown_signal() => {}
            _ = updated => restarting.set(true),
        }
    };
    mis_orchestrator::serve_until(listener, app, stop).await?;
    if restarting.get() {
        tracing::info!("restarting on the new build");
        return Err(mis_update::restart(&exe));
    }
    Ok(())
}

/// This machine's name, as its worker reports it: `MIS_HOSTNAME`, else `hostname`'s.
fn hostname() -> String {
    if let Some(h) = std::env::var("MIS_HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
    {
        return h;
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Ctrl-C, or SIGTERM (`kill`, service managers) on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    #[test]
    fn channel_defaults_to_stable_only_for_ci_builds() {
        use super::{channel, UpdateChannel};
        use mis_types::Channel;
        assert_eq!(channel(None, true), Some(Channel::Stable));
        assert_eq!(channel(None, false), None);
        assert_eq!(
            channel(Some(UpdateChannel::Edge), false),
            Some(Channel::Edge)
        );
        assert_eq!(channel(Some(UpdateChannel::Off), true), None);
    }

    #[test]
    fn env_file_arg_as_clap_reads_it() {
        let arg = |a: &[&str]| super::env_file_arg(&a.iter().map(Into::into).collect::<Vec<_>>());
        assert_eq!(arg(&["m", "doctor", "--env-file", "f"]), Some("f".into()));
        assert_eq!(arg(&["m", "--env-file=f", "--json"]), Some("f".into()));
        assert_eq!(arg(&["m", "--env-file"]), None);
        assert_eq!(arg(&["m", "--", "--env-file", "f"]), None);
        assert_eq!(arg(&["--env-file"]), None, "argv[0] is the program");
    }

    #[test]
    fn token_only_on_a_terminal() {
        use super::token_env;
        assert_eq!(token_env(Some("t0k"), true), " MIS_TOKEN='t0k'");
        assert_eq!(token_env(Some("t0k"), false), " MIS_TOKEN=<redacted>");
        assert_eq!(token_env(None, false), "");
    }

    #[test]
    fn quotes_shell_metacharacters() {
        assert_eq!(super::sh_quote("abc"), "'abc'");
        assert_eq!(super::sh_quote("a$b c"), "'a$b c'");
        assert_eq!(super::sh_quote("it's"), "'it'\\''s'");
    }
}
