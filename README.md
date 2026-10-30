# Make It So

Coding-agent orchestration. See [SPEC.md](SPEC.md) for the vision.

Current state: text-only vertical slice.

```
mis (cli)  --ws-->  mis-orchestrator  <--ws--  mis-worker  --spawns-->  claude -p
```

## Crates

- `mis-types` — wire types (JSON over WebSocket)
- `mis-agent` — `Agent` trait + Claude Code and Codex CLI backends
- `mis-orchestrator` — worker ledger, task queue, event fan-out
- `mis-worker` — runs assigned tasks via `mis-agent`
- `mis-cli` — `mis status | task | show | tail`
- `mis-tray` — macOS menu bar / Windows tray app

## Install

macOS / Linux (installs `mis`, `mis-worker`, `mis-orchestrator` to `~/.local/bin`):

```sh
curl -fsSL https://raw.githubusercontent.com/tommy-xr/MakeItSo/main/install.sh | sh
```

Windows (PowerShell; installs to `%LOCALAPPDATA%\mis\bin` and adds it to user PATH):

```powershell
irm https://raw.githubusercontent.com/tommy-xr/MakeItSo/main/install.ps1 | iex
```

Knobs: `MIS_VERSION=v0.1.0` pins a release (default latest); `MIS_INSTALL_DIR=...` changes the
install dir.

From source: `cargo install --git https://github.com/tommy-xr/MakeItSo mis-worker` (likewise
`mis-cli`, `mis-orchestrator`).

## Try it

```sh
cargo build
./target/debug/mis dev --skip-permissions  # orchestrator + worker on 127.0.0.1:7474
./target/debug/mis task "list files in /tmp" --follow
./target/debug/mis task "say hi" --harness codex --follow
./target/debug/mis task "run the tests" --wait  # prints the summary; exit 0 done, 1 failed, 2 no result (--json: the task)
./target/debug/mis status
./target/debug/mis show 1a2b3c4d                # one task's details + transcript (id prefix from status)
```

Separate processes (what `mis dev` runs for you):

```sh
export MIS_TOKEN=secret                    # optional; omit on both sides to disable auth
./target/debug/mis-orchestrator            # 127.0.0.1:7474
./target/debug/mis-worker --skip-permissions
```

## Menu bar app

```sh
cargo run -p mis-tray            # same --url/--token (MIS_URL/MIS_TOKEN) as `mis`
cargo run -p mis-tray -- --print # print the menu once as text and exit
```

Shows the fleet and the 8 newest tasks; click one to open it in the browser. Without
`MIS_URL`/`MIS_TOKEN` it uses `MIS_BIND`/`MIS_TOKEN` from `~/.config/mis/orchestrator.env`.

## Set up a worker

Run once per machine, from the directory tasks should run in:

```sh
mis-worker setup                 # asks url, token, roots; test-connects; saves config
mis-worker --skip-permissions    # then runs from the saved config
```

Every question has a flag (`--url`, `--token`, `--cf-access-client-id`, `--root`), so it also runs
unattended. The Access secret is never a flag: it comes from `CF_ACCESS_CLIENT_SECRET` or a hidden
prompt. Config lives in `~/.config/mis/worker.toml` (or `$MIS_STATE_DIR`); flags and env override it:

```toml
url = "wss://mis.example.com"
token = "..."
cf_access_client_id = "..."      # optional, with the secret
cf_access_client_secret = "..."
roots = ["/Users/me/code"]       # task --cwd must start under one (not a sandbox); default: cwd
```

Setup also asks what the machine is for (`--purpose "Blender + blender-mcp; 3D assets"`) and takes an
inventory: MCP server names per harness (`~/.claude.json`, `.mcp.json` under the roots,
`~/.codex/config.toml`; names only, never commands, args, env or URLs), known apps such as Blender,
Godot, Xcode, ffmpeg, and CPU / RAM / GPU. Both go to the orchestrator and show under Workers. Each start
re-takes the inventory and saves it in `worker.toml` when it changed (the file is rewritten, so comments are lost).
The planner routes on both: a task that needs a worker's tools waits for that worker
("waiting for studio") instead of running elsewhere. Edit the note from the worker's page; when the
worker gains apps or MCP servers, the page lists them and **Suggest note** drafts an update for you
to edit and save.

## Remote machines

Start the orchestrator with `--bind 0.0.0.0:7474` and a `MIS_TOKEN`. It prints a
ready-to-paste `MIS_URL=... mis-worker` line for each LAN address (and for `--public-url` if set).

Behind a Cloudflare Tunnel with an Access service-auth policy, give `mis` and `mis-worker` the
service token too (`mis` also takes `--cf-access-client-id` / `--cf-access-client-secret`; `mis-worker`
reads the secret from env only):

```sh
export CF_ACCESS_CLIENT_ID=<id>.access
export CF_ACCESS_CLIENT_SECRET=<secret>
```

Requests with a browser `Origin` are rejected unless it is the orchestrator's own page (below) or listed
via `--allowed-origin https://mis.example.com` (repeatable, or comma-separated `MIS_ALLOWED_ORIGINS`).
Browsers send the token as WebSocket subprotocols `bearer, <token>`, so keep it to letters, digits
and `-._` (e.g. `openssl rand -hex 24`).

For read-only access (a dashboard, a teammate) add tokens in `MIS_READ_TOKENS` (comma-separated; or
`--read-token`, though argv is visible to other local users). They load the page, status, transcripts
and `/mcp`, but submitting or replying is refused and workers can't connect with them; the page hides
its composers. They need `MIS_TOKEN` set too. A Cloudflare Access login is always full access.

## Behind Cloudflare Access

With a Cloudflare Tunnel (`cloudflared`, ingress -> `http://localhost:7474`), put an Access app in front
and have the orchestrator check the JWT Access adds to each request.

In the Zero Trust dashboard:

1. Access -> Service credentials -> Service Tokens: create one for workers and `mis`.
2. Access -> Applications -> Add -> Self-hosted, for the tunnel's hostname, with two policies:
   *Allow* with *Emails* = you (phone browser), and *Service Auth* with that service token.
3. Copy the app's *Application Audience (AUD) Tag*; the team name is the `<team>` in
   `<team>.cloudflareaccess.com` (Settings -> Team name and domain).

```sh
mis-orchestrator --cf-access-team <team> --cf-access-aud <aud-tag>   # or MIS_CF_ACCESS_TEAM / _AUD
CF_ACCESS_CLIENT_SECRET=<secret> mis-worker setup --url wss://mis.example.com \
  --cf-access-client-id <id>.access
```

Requests with a valid Access JWT need no `MIS_TOKEN`, so the page skips the token prompt. Requests
without one are rejected unless they come straight from loopback with no Cloudflare headers;
those still follow `MIS_TOKEN`. So any local process can still connect, as without Access; set
`MIS_TOKEN` too to close that. The JWT is checked when a socket opens, not again while it stays open.


The orchestrator serves a text-only page at `http://<host>:7474/`: tasks (list, submit, transcripts) and workers.
Open it and paste the token (set `MIS_TOKEN`; without one, only `localhost` pages may connect). To reach
it from a phone:

- `cloudflared tunnel --url http://localhost:7474`
- `tailscale serve 7474`

## Task history

The orchestrator saves its tasks (status, transcript, summary, evidence, attention) in
`<state-dir>/ledger.json` and `<state-dir>/transcripts/`, readable only by you, at most once a second
and on Ctrl-C / SIGTERM, and restores them on start. Tasks are forgotten 30 days after they end.

A restart drops every connection. A worker's agent keeps running meanwhile; if the worker reconnects
within the grace period (60 s) still running its task, the task carries on, otherwise it fails.
Tasks still being planned fail. An unreadable `ledger.json` is moved to `ledger.json.bad-<time>` and
the orchestrator starts empty. A second orchestrator on the same state dir refuses to start.

## Push notifications

Workers -> *notify me* pushes a notification when a task fails, hits a usage limit, or finishes;
tapping it opens the task. On iPhone (iOS 16.4+) add the page to the Home Screen first (Share ->
Add to Home Screen) and toggle it there; desktop Chrome/Firefox work in a tab. It needs HTTPS (or
`localhost`), and is hidden for read-only tokens.

The orchestrator keeps its VAPID key and the subscriptions in `--state-dir` / `MIS_STATE_DIR`
(default `~/.local/state/mis`, `%LOCALAPPDATA%\mis` on Windows), readable only by you. Set
`MIS_VAPID_SUBJECT` to a `mailto:` or `https:` contact for push services (default: this repo's URL).

Privacy: a notification carries only the task's status, a clipped first line of its prompt, and how
long it took. It travels through the browser vendor's push service (Apple, Google or Mozilla),
end-to-end encrypted (RFC 8291): the service sees when and how much, not what.

## Image attachments

`POST /api/upload` with a raw JPEG, PNG, WebP or GIF body (up to 10 MB; read-only tokens get 403)
returns `{"id": ...}`; a `submit` may attach up to 4 ids as `images`. The worker writes them to a
temp dir for the run: codex gets `-i <file>` each, claude a prompt line naming the files. Uploads
live in `<state-dir>/images/` and are deleted after 7 days, attached or not.

On the page, the paperclip beside the mic attaches up to 4 photos (camera or library on a phone),
shrunk to 1600 px JPEG before upload; a task's photos show in its view.
