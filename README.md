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
- `mis-cli` — `mis status | task | tail`

## Try it

```sh
cargo build
./target/debug/mis dev --skip-permissions  # orchestrator + worker on 127.0.0.1:7474
./target/debug/mis task "list files in /tmp" --follow
./target/debug/mis task "say hi" --harness codex --follow
./target/debug/mis status
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
roots = ["/Users/me/code"]       # task --cwd must resolve under one; default: cwd
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
