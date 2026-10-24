# Make It So

Coding-agent orchestration. See [SPEC.md](SPEC.md) for the vision.

Current state: text-only vertical slice.

```
mis (cli)  --ws-->  mis-orchestrator  <--ws--  mis-worker  --spawns-->  claude -p
```

## Crates

- `mis-types` — wire types (JSON over WebSocket)
- `mis-agent` — `Agent` trait + Claude Code backend (`stream-json`)
- `mis-orchestrator` — worker ledger, task queue, event fan-out
- `mis-worker` — runs assigned tasks via `mis-agent`
- `mis-cli` — `mis status | task | tail`

## Try it

```sh
cargo build
export MIS_TOKEN=secret                    # optional; omit on both sides to disable auth
./target/debug/mis-orchestrator            # 127.0.0.1:7474
./target/debug/mis-worker --skip-permissions
./target/debug/mis task "list files in /tmp" --follow
./target/debug/mis status
```

Remote machines: `--bind 0.0.0.0:7474` on the orchestrator, `MIS_URL=ws://host:7474` on workers/CLI.
