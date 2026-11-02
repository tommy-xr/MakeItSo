# AGENTS.md

Guidance for agents (and humans) working in this repo.

## Architecture: functional core, imperative shell

Keep logic pure and push I/O to the edges.

- **Core**: scheduling, ledger transitions, event parsing, protocol types. Plain
  functions and data: `fn step(state, input) -> (state, effects)`. No sockets,
  processes, clocks, or logging inside. Unit-test these directly.
- **Shell**: WebSocket handlers, process spawning, filesystem, CLI output. Thin
  code that feeds inputs to the core and performs the effects it returns.

Signs you are mixing layers: a function that both mutates shared state and
sends on a socket; a parser that spawns a process; a test that needs a live
server to check a state transition.

Agent backends follow this already: `parse_line(&str) -> Vec<AgentEvent>` is
core, the process runner around it is shell. Match that shape.

## Conventions

- Wire types live in `mis-types`; every message is a tagged enum.
- Each crate's README/doc comment states its one responsibility.
- Tests: pure logic in unit tests; one end-to-end test per behavior in
  `mis-orchestrator/tests`.
- Run `cargo fmt`, `cargo clippy -D warnings`, `cargo test` before pushing.
- Commits and PR titles use Conventional Commits: `feat:`, `fix:`, `ci:`, `docs:`,
  `refactor:`, `test:`, `chore:`. Imperative, lower-case subject, no period.

## PR evidence

- Every PR body shows before/after proof: output, logs, measurements.
- UI changes add before/after screenshots at phone width (390 px, dark), from the
  base branch and the PR branch with the same data; a GIF for flows.
- Synthetic data only: fixture sessions, tasks and routines in temp dirs
  (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `--state-dir`, `--routines-dir`,
  `MIS_STATE_DIR`) and `MIS_HOSTNAME=demo-mac`. Never real sessions, transcripts,
  task history, paths or hostnames from the machine.
- Host images with `scripts/pr-media "<desc>" before.png after.png flow.gif`
  (secret gist; prints Markdown). A GIF from stills:
  `ffmpeg -framerate 1 -i f%d.png -vf "split[a][b];[a]palettegen[p];[b][p]paletteuse" flow.gif`.
