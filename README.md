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
- `mis-update` — finds, checks (minisign) and installs signed release builds
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

Updates: a released worker switches to its orchestrator's exact version, downloading that build
from GitHub Releases, checking its signature against `release/minisign.pub`, and restarting once
its task ends (`--no-self-update` / `MIS_NO_SELF_UPDATE=1` turns that off). The orchestrator
follows `--update-channel stable|edge|off` (stable for released builds): every 5 min it looks for
a newer release, checks it the same way, runs the new binary's `--self-test`, and once no task
runs restarts on it. The page's title bar shows a pending or failed update. Builds not made by CI
never update; `mis-up` turns both off.

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
mis-worker setup                 # asks url, token, roots, permissions; test-connects; saves config
mis-worker                       # then runs from the saved config
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

[permissions]                    # what agents may do without asking
mode = "ask"                     # folders + ask on the phone; "folders": deny the rest; "full": anything
folders = ["~/code/game", "~/assets"]  # writable besides the task's dir; default: the roots
mcp = ["blender"]                # MCP servers whose tools are allowed; default: all found
commands = ["python3", "git log"]      # claude: command prefixes allowed besides read-only ones; default: none
```

Without `[permissions]`, claude and codex run with their headless defaults, which deny writes, MCP
calls and all but read-only shell commands; the Workers view warns about it. `--skip-permissions` means `mode = "full"`
whatever the file says (`--permissions`, `--folder`, `--mcp`, `--command` answer setup's questions).
In folders mode claude gets `--permission-mode acceptEdits`, `--add-dir` per folder,
`--allowedTools mcp__<server>__* Bash(<command> *)` and its Bash sandbox, so an allowed `python3`
can't write outside the folders either (network stays open; on Linux the sandbox needs `bubblewrap`
and `socat`, or claude refuses to start). Anything else is denied. Codex gets its workspace-write
sandbox with the folders writable and network on, and the listed MCP servers approved; it runs any
command inside that sandbox. Both may also write a temp dir. Allow rules in your own claude or codex
config still apply, and MCP servers run outside either sandbox. A claude MCP server loads in tasks
when it is user-scoped (`claude mcp add -s user`) or set up for the task's directory.

In ask mode, whatever the rules don't allow is asked instead of denied (claude only: codex has no hook,
so it runs as folders). The task shows a card with **Allow**, **Deny** and **Always**, and a push says
"approval needed · <task>: <what>". Always saves a rule to `worker.toml`: the written file's folder,
the command's program (`python3`), or the MCP server. No answer in 30 minutes is a deny. A worker
re-asks after the orchestrator restarts.

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
its composers. They need `MIS_TOKEN` set too. A Cloudflare Access login is full access (see
`MIS_ACCESS_SUBJECTS` below to limit whose).

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
MIS_ACCESS_SUBJECTS=you@example.com,<id>.access mis-orchestrator ...  # only these get in
CF_ACCESS_CLIENT_SECRET=<secret> mis-worker setup --url wss://mis.example.com \
  --cf-access-client-id <id>.access
```

`MIS_ACCESS_SUBJECTS` (or repeated `--access-subject`) lists the emails and service token client
ids let in; any other identity the Access app admits is refused as if it had no JWT. Unset, anyone
the Access app admits gets full access, so set it before adding people to that Access app or
sharing its policy with another app.

Requests with a valid Access JWT need no `MIS_TOKEN`, so the page skips the token prompt. Requests
without one are rejected unless they come straight from loopback, or from a private-network
address (e.g. a LAN worker at `ws://192.168.x.y:7474`) with `MIS_TOKEN`, with no Cloudflare
headers; those still follow `MIS_TOKEN`. "Private" is judged by the TCP peer, so don't put a
proxy that relays public traffic from a private address in front of the orchestrator. So any local process can still connect, as without Access; set
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

## Capture

`POST /api/capture`, `multipart/form-data` (an iOS Shortcut's *Get Contents of URL* -> *Form*):
optional `url` (http/https), `text`, up to 4 `image` files (JPEG, PNG, WebP or GIF by content, 10 MB
each) and `via` (default `shortcut`); at least one of url, text, image. Values are trimmed, blank
fields and empty files count as absent, and of several URLs (one per line) only the first is kept.
Each capture is written to `<state-dir>/captures/inbox/YYYY-MM-DD-<slug>.md` (YAML frontmatter, then the text) with its
images beside it (`…-1.jpg`) before the reply `{"id", "file"}`: 201, or 200 with the first capture
when the same url, text and images arrive again within 10 minutes. The slug is the text's first
words, else the URL's host and last path segment; the first `#tag` in the text becomes `project`.
Captures are kept: never swept or edited. iPhone photos are HEIC, which is refused (415): have the
Shortcut *Convert Image* to JPEG (and resize, e.g. 1600 px) first.

Capture tokens (`MIS_CAPTURE_TOKENS`, comma-separated, or `--capture-token`; need `MIS_TOKEN`) may
call `POST /api/capture` and nothing else. The full token and an Access login may capture too; read
tokens may not. Behind Access, add a second Access app on path `/api/capture` with a *Service Auth*
policy for a capture-only service token, and pass its AUD as `--cf-access-capture-aud` /
`MIS_CF_ACCESS_CAPTURE_AUD`. Its JWT is accepted only there, still needs a capture (or full) token,
and grants no more than capturing, so keep the main app's policies naming their own service tokens. A capture token is
never upgraded by an Access login.

## Apps

The orchestrator can host small static web apps (a bundle of HTML, JS, CSS, images) for you and
your household, served on a second listener with no orchestrator routes on it:

```sh
MIS_APPS_BIND=127.0.0.1:7475 mis-orchestrator   # or --apps-bind; unset: apps aren't served
mis apps publish pool ./dist --activate          # dist/app.json + the files
mis apps list                                    # releases, which one is live
mis apps activate pool 1                         # revert: activate any earlier release
```

A bundle has `app.json` at its top: `{"title": "Pool scores", "icon": "icon.png", "clients":
{"web": "index.html"}}` (`icon` optional; `web` is the page browsers open). The server checks
every upload and refuses links, devices, absolute or `..` paths, hidden files, more than 2000 files
or 20 MiB, and an `app.json` naming files it doesn't hold. It never runs app code. `mis apps
publish` leaves hidden files (`.DS_Store`, `.git`) out; with macOS `tar`, set `COPYFILE_DISABLE=1`
so it adds no `._*` files. `web` must be at the bundle's top.

Each publish is a new numbered release, all kept, in `<state-dir>/apps/<name>/releases/<n>/`;
`<state-dir>/apps/<name>/active` holds the live n. Names are `a-z`, `0-9` and `-`, up to 32.
`POST /api/apps/<name>/releases` (raw `.tar.gz` body, `?activate=true` to go live) and
`POST /api/apps/<name>/activate` (`{"release": n}`) need the full token; `GET /api/apps` lists
them for read-only tokens too. Menu -> Apps shows each app and who it's shared with, links out to
the live one and asks before making a newer release live or removing someone.

The apps listener serves the live release at `/a/<name>/` (paths without an extension fall back to
the entry page, for client-side routes) and a list of live apps at `/a/`. Every response sends
`Cache-Control: no-cache` and a CSP that allows scripts, styles, fonts and connections only from
this origin (bundle dependencies; no CDNs). All apps share the origin and its storage: they are not
isolated from each other. An entry page at `/a/<name>/scores/2026` resolves relative URLs from
`scores/`, so client-side routers should use absolute asset paths (`/a/<name>/...`) or hash routes. Set `--apps-public-url` / `MIS_APPS_PUBLIC_URL` (e.g.
`https://home.example.com`) so app links point there (needed when it binds `0.0.0.0`).

### Who sees an app

Each app opens only for its members, in `<state-dir>/apps/<name>/policy.json` (outside every
bundle): `{"members": {"spouse@example.com": "editor"}}`. Viewers read, editors also write, owners
also administer. Emails match ignoring case. An app shared with no one opens for nobody but
direct loopback requests.

```sh
mis apps share pool spouse@example.com --role editor   # viewer (default), editor or owner
mis apps unshare pool spouse@example.com
mis apps list                                          # members under each app
```

`POST /api/apps/<name>/members` (`{"email", "role"}`, `role: null` removes) needs the full token.
The apps listener answers only to the host of `--apps-public-url` or a loopback name (anything
else: 421, so a DNS-rebound name gets nothing). Who is asking:

- a valid JWT of the home Access app (`--apps-cf-access-aud`): that email. The MIS Access app's
  JWT is refused here, and this one is refused by the orchestrator;
- a direct loopback request with no Cloudflare headers: `local`, owner of every app (dev, tests);
- anything else (through the tunnel without a JWT, or from the LAN): 401.

`/a/` lists only your apps; another app's paths answer 403. `GET /a/<name>/api/v1/me` returns
`{"user", "role", "app"}`. `/a/<name>/api/` belongs to the server, not the bundle; its routes take
only GET and POST.

### Household setup

1. Tunnel: add an ingress rule `home.<domain>` -> `http://localhost:7475` beside the MIS one.
2. Access -> Applications -> Add -> Self-hosted for `home.<domain>`: a policy *Allow* with
   *Emails* you and your spouse, login by Google or one-time PIN, session ~1 month. **Never reuse
   an Access policy or group between the MIS and home Access apps**: adding someone to the
   household must never let them into MIS.
3. Run the orchestrator with that app's AUD (same team as `MIS_CF_ACCESS_TEAM`):

   ```sh
   MIS_APPS_BIND=127.0.0.1:7475 MIS_APPS_PUBLIC_URL=https://home.<domain> \
   MIS_APPS_CF_ACCESS_AUD=<home-aud-tag> mis-orchestrator --cf-access-team <team> --cf-access-aud <aud-tag>
   ```

4. `mis apps share <name> <email> --role editor` for each person and app.
