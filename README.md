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
./target/debug/mis status
./target/debug/mis show 1a2b3c4d                # one task's details + transcript (id prefix from status)
```

Separate processes (what `mis dev` runs for you):

```sh
export MIS_TOKEN=secret                    # optional; omit on both sides to disable auth
./target/debug/mis-orchestrator            # 127.0.0.1:7474
./target/debug/mis-worker --skip-permissions
```

## Set up a worker

Run once per machine, from the directory tasks should run in:

```sh
mis-worker setup                 # asks url, token, roots; test-connects; saves config
mis-worker --skip-permissions    # then runs from the saved config
```

Every question has a flag (`--url`, `--token`, `--cf-access-client-id`, `--cf-access-client-secret`,
`--root`), so it also runs unattended. Config lives in `~/.config/mis/worker.toml` (or
`$MIS_STATE_DIR`); flags and env override it:

```toml
url = "wss://mis.example.com"
token = "..."
cf_access_client_id = "..."      # optional, with the secret
cf_access_client_secret = "..."
roots = ["/Users/me/code"]       # task --cwd must start under one (not a sandbox); default: cwd
```

## Remote machines

Start the orchestrator with `--bind 0.0.0.0:7474` and a `MIS_TOKEN`. It prints a
ready-to-paste `MIS_URL=... mis-worker` line for each LAN address (and for `--public-url` if set).

Behind a Cloudflare Tunnel with an Access service-auth policy, give `mis` and `mis-worker` the
service token too (or pass `--cf-access-client-id` / `--cf-access-client-secret`):

```sh
export CF_ACCESS_CLIENT_ID=<id>.access
export CF_ACCESS_CLIENT_SECRET=<secret>
```

Requests with a browser `Origin` are rejected unless it is the orchestrator's own page (below) or listed
via `--allowed-origin https://mis.example.com` (repeatable, or comma-separated `MIS_ALLOWED_ORIGINS`).
Browsers send the token as WebSocket subprotocols `bearer, <token>`, so keep it to letters, digits
and `-._` (e.g. `openssl rand -hex 24`).

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
mis-worker setup --url wss://mis.example.com \
  --cf-access-client-id <id>.access --cf-access-client-secret <secret>
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
