---
name: projects
description: Start a new Make It So project (a record in the private mis-notes repo) from a description, optionally with a new private GitHub repo, delivered as a mis-notes PR the owner merges. Use only when asked to start a Make It So project.
---

# Starting a project

A project is a folder `projects/<slug>/` in the private mis-notes repo whose `README.md` is its
record: frontmatter, then a short description. Make It So lists projects from those records once
they are on mis-notes `main`. You deliver a PR adding the record (and, when asked, a new private
GitHub repo for the project's code); the owner merges it. Nothing you do lands on mis-notes `main`.

Your request names the slug, the title, the description, whether to create a GitHub repo, and the
record's frontmatter. Use them as given.

## Hard rules

- Never merge a PR, never commit or push to mis-notes `main`, never force-push.
- Never delete, overwrite or reuse a repo or folder that exists. If the slug is taken anywhere,
  stop and say where. Any check that errors (network, auth) rather than answering: stop and say so.
- A new repo is always private (`--private`), owned by your own gh user, and named `<slug>`.
- Work in your own mis-notes worktree (step 2), never in the checkout you start in (others use it)
  or the orchestrator's `<state-dir>/notes` clone.
- No title, description or other text from your request ever goes in a shell command: a quote
  in it breaks the command. Write such text into files with your file-writing tool; commands use
  only the slug, the owner and paths.
- Shell variables don't carry from one command to the next: write the owner and paths out in
  full in every command.
- The repo's README is neutral: what the project is, goals, first steps. No private paths, notes,
  names of machines or anything from mis-notes beyond the description you were given.

## Steps

1. Check; nothing is written until every check passes.
   - `gh auth status` passes.
   - From the checkout you start in, `git fetch origin`, then read the folder names and each
     `README.md` frontmatter (`id`, `aliases`) under `projects/` and `archive/` on `origin/main`
     (`git ls-tree --name-only origin/main projects/ archive/`,
     `git show origin/main:projects/<x>/README.md`). No folder, id or alias may equal `<slug>`,
     ignoring case.
   - `git ls-remote --exit-code origin refs/heads/project-<slug>` finds nothing (exit 2).
   - With a repo also: note your login, `gh api user -q .login` (the owner below). `gh repo view
     <owner>/<slug>` fails with "Could not resolve". The repo's folder is the checkout's parent
     folder plus `/<slug>` (`dirname "$(git rev-parse --show-toplevel)"`), and it doesn't exist.
2. Make your worktree off `origin/main`:
   `git worktree add -b project-<slug> ../mis-notes-project-<slug> origin/main`, and work there.
3. Repo (only if asked). `mkdir <folder>`; if it fails, stop. With your file-writing tool, write
   `<folder>/README.md`: `# <title>`, what it is, `## Goals` and `## First steps`, from the
   description; plain and short. Then one command, so it stops at the first failure:
   `cd <folder> && git init -b main && git add README.md && git commit -m "docs: add readme" &&
   gh repo create <owner>/<slug> --private --source . --remote origin --push`.
   `gh repo view <owner>/<slug> --json visibility -q .visibility` must say `PRIVATE`; if not, stop
   and say so.
4. Record. In the worktree, write `projects/<slug>/README.md`: the frontmatter from your request
   exactly (with a repo, `OWNER` in `repos:` replaced by the owner; without one, `repos: []`), a
   blank line, then the description as given. Nothing else in the folder.
5. Commit (`feat: add project <slug>`), `git push -u origin project-<slug>`, then
   `gh pr create --base main --title "feat: add project <slug>" --body "Adds project <slug>."`
   (with a repo: `--body "Adds project <slug>, repo https://github.com/<owner>/<slug>."`).
   Leave it for the owner to merge. Then remove your worktree:
   `git worktree remove ../mis-notes-project-<slug>` (the branch is pushed).
6. Reply with the PR link and, if made, the repo link
   (`https://github.com/<owner>/<slug>`). If a step after the repo failed, say what exists now
   (the repo, the branch) and leave it; never delete it.

## Record format

```
---
schema: 1
id: <slug>
title: '<title>'
aliases: []
status: active
repos: [<owner>/<slug>]
notes: []
---

<description>
```

`id` must equal the folder's name; in `title`, each `'` is doubled (`'Bryan''s tool'`).
`aliases` are other names it goes by; `repos` are GitHub `owner/repo`s; `notes` are paths to the
owner's own notes (leave empty). Once the PR is merged, the project shows in Make It So's Projects
view.
