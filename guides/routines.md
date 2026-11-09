---
name: routines
description: Write a Make It So routine (a Markdown file whose prompt MIS runs as a task on a schedule), covering its frontmatter keys, every/cron schedules, report, dry-run and live modes, and merging PRs through merge_pr. Use when asked to add, change or debug a routine.
---

# Make It So routines

A routine is one `<name>.md` file in the orchestrator's routines folder (default
`~/.config/mis/routines/` on the orchestrator's machine, or its `--routines-dir`). MIS re-reads
the folder every 30 s and starts each due routine as an ordinary task. `mis routines` lists them,
with why a file is bad (a bad file never runs).

## File

```markdown
---
every: 6h                 # or cron: "0 9 * * 1-5"; exactly one of the two
harness: claude           # optional; default claude (or codex)
model: sonnet             # optional, passed as is; default the harness's
worker: build-mac         # optional: only this worker runs it (its name or hostname)
cwd: code/make-it-so      # optional: folder on the worker (see below)
paused: true              # optional; start new files paused
act: true                 # optional: may act once switched live (see Modes)
merge:                    # optional, needs act: true (see Merging)
  repos: [owner/name]
  authors: [app/dependabot]
  max_files: 10
  max_lines: 300
---
The prompt each run gets.
```

- Name: the file name less `.md`, or `name:`; letters, digits, `-`, `_`, `.`, starting with a
  letter or digit; unique across files.
- One `key: value` per line; quotes optional; ` #` starts a comment; lists are `[a, b]` (no
  `- item` lines, no commas inside quotes). Only `merge:` takes indented keys. Unknown or
  repeated keys, or an empty prompt, make the file bad.
- `cwd`: absolute, `~/…` (the worker's home) or relative to the worker's first root, any way
  under one of its roots. A bad one fails each run.
- `paused`: Pause or Resume in the Routines view overrides the file's value from then on.
- `every`: `<n>m`, `<n>h` or `<n>d`, at least 1m, counted from the last run; the first run is
  as soon as MIS sees the file, so write new files with `paused: true` and let the owner resume.
- `cron`: `minute hour day month weekday`, each `*`, `n`, `a-b`, optional `/step`,
  comma-separated; no names; weekday 0-7 (0 and 7 are Sunday); with both day fields set, either
  matches. Orchestrator's local time. Quote it.
- After downtime one run starts, not one per missed slot; a slot that falls while a run is still
  going is skipped; time spent paused is never caught up.

## Modes

Every run's prompt starts with MIS's rules for its mode; write the file's prompt for the mode.

- Report (no `act`): the run is read-only. It investigates and reports; no pushes, PR or issue
  changes, deploys or other remote state.
- Dry run (`act: true`, not yet live): lists every action it would take, with full text, and
  takes none.
- Live: only the owner switches it on (Routines view "Go live"); never run `mis routines live`
  yourself. Runs act as the prompt says, and only that far, and report each action. Live
  approves this exact version: editing the prompt, `merge`, `harness`, `model`, `worker` or
  `cwd` drops it back to dry run. `mis routines dry-run <name>` does that by hand.
- No run may push tags, create releases, or read or print secrets; none merges but through
  `merge_pr`.

## Merging

Without `merge:`, runs never merge. With it, live runs merge only by calling the `merge_pr` tool
(repo, number), which checks the policy in code; in a dry run it only says whether it would.

- Required: `repos` (`owner/name`), `authors` (logins as `gh` gives them, e.g.
  `app/dependabot`), `max_files` (1-100), `max_lines`.
- Optional: `forbid_paths` (globs: `*` within a segment, `**` across; no `[]` or `{}`),
  `require_labels` (all of them), `method` (`squash`, `merge` or `rebase`; default squash),
  `approve` (`phone`: the user approves each merge on the phone, the default; `none`).
- `merge_pr` also refuses drafts, PRs whose checks failed, are pending or never ran, ones not
  cleanly mergeable and, with `forbid_paths` set, any that renames or copies a file. Claude runs
  are blocked from merging any other way; codex is only told.

## Writing the prompt

- Each run starts fresh and unattended: name the repo, what to look at, what to do, what to
  report. Keep it to what the mode allows.
- New routines from the phone (Routines, +) start paused and report-only.
- Try it: `mis routines run <name>` starts one run now; the task shows what it did.
