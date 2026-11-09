---
name: apps
description: Build or change a Make It So app (a static web bundle MIS serves at /a/<slug>/ with its own records), from source in the mis-notes repo to a staged release and a PR the owner approves. Use for any request to make, change or fix a household app.
---

# Make It So apps

An app is a static bundle (`app.json`, `index.html`, assets) that MIS serves at `/a/<slug>/` on its
apps listener, with records in a store of its own. Its source is `apps/<slug>/` in the private
mis-notes repo. You deliver a PR there and a staged release; the owner reviews both and makes it
live. Nothing you do goes live.

## Steps

1. From the mis-notes checkout you start in, make your own worktree off `origin/main` and work
   only there (others use the checkout; never the orchestrator's `<state-dir>/notes` clone):
   `git fetch origin && git worktree add -b <slug>-<what> ../mis-notes-<slug>-<what> origin/main`.
   Never commit to `main`, never force-push.
2. New app: from the worktree's root, `mis apps new <slug>` (copies `apps/_template`, puts the
   SDK in `sdk/`; slug: `a-z 0-9 -`, up to 32), then `cd apps/<slug> && npm install`. Read its
   README. A change: work in `apps/<slug>/`; `npm run sdk` refreshes `sdk/`.
3. Replace the template's sample `items` with the app's own model and screens: pure logic in
   `src/domain/` with vitest tests in `tests/`, screens in `src/ui/`, state over the SDK in
   `src/store.ts`, invented sample data in `src/mock-seed.ts`. Update the app's README (data
   model, commands).
4. `npm test && npm run build && npm run shots` until all pass (see Verify).
5. `mis apps publish <slug> dist`: a new staged release on the live orchestrator, the one write
   to it you may make. Never `--activate` there, and never `activate`, `share`, `unshare` or
   `restore`. If it is refused (no token), say so and leave publishing to the owner.
6. Commit (`feat: …`/`fix: …`), push the branch, open a PR in mis-notes (see PR).
7. Report the PR URL, the release number and what the owner should try before making it live.

## SDK

`mis` (types in `sdk/mis.d.ts`), loaded by `index.html` from `/a/_/mis.js`; never put it in the
bundle. Use it for every call: the API is GET and POST only, and writes must come from the app's
own page.

- A record is `{id, rev, by, at, data}`; `data` is JSON up to 64 KiB. Put `v: 1` in it.
- Ids: 1-128 of `A-Z a-z 0-9 . _ : -`, so slugify what goes in one (an email can't). Collections:
  `a-z 0-9 _ -`, up to 64. Query fields: top-level `data` keys of `A-Z a-z 0-9 _`.
- Every write sends the `rev` it read; `0` means create only. Stale: a `MisError` with status 409
  and `current` (null: it was removed). Show it, offer keep theirs / save mine (template
  `Conflict.tsx`); never retry blindly. An identical retry after a lost reply succeeds;
  removing a missing record succeeds; `get` of a missing one is `null`.
- Imported or derived records get deterministic ids (a content hash, `interest-<person>-<yyyy-mm>`)
  written with rev 0, so retries and two open phones never write twice.
- Spaces: `shared` (every member) and `me` (only the writer's). Roles: `owner`, `editor` write;
  `viewer` reads only: give viewers a read-only UI, and show a 403 as "you can only view".
- `records.batch(ops)`: up to 100 ops, each id once, all or nothing; a refusal carries `index`.
  Use it for imports and anything that must land together.
- `records.list(space, collection, q)`: `where` with `eq`/`gt`/`gte`/`lt`/`lte` (a bound matches
  only values of its JSON type), one `order_by`, `limit` up to 100; follow `cursor` to the end.

## Bundle rules

- Vite `base: '/a/<slug>/'`, hash routes (`#/screen`), absolute asset paths (`base-uri` is `none`).
- CSP: scripts from the bundle only (no CDN, no inline script), `connect-src 'self'`: no external
  fetches, fonts or analytics. Bundle every dependency. `vite preview` and shots apply this CSP.
- Up to 20 MiB and 2000 files; no symlinks, dot files or top-level `api/`.
- Every app shares one origin: localStorage is visible to all of them. Prefix keys with the slug;
  keep nothing secret there.
- No offline: a failed write shows "not saved" and keeps the input; a failed refresh keeps
  screens and drafts; a stale load never overwrites a newer write (template `store.ts`).
- A double tap must not add twice: stable id per draft, button busy while saving.
- Phone first: 390 px wide, light and dark, safe-area insets, keep the web manifest and icons.

## Verify

- `npm test`, `npm run build` (type-check + `dist/`).
- `npm run shots`: serves `dist/` in mock mode (`?mock=1`, in memory) with the listener's CSP,
  screenshots every screen at 390 px light and dark into `shots/` (gitignored), and fails on
  console errors, CSP violations and HTTP errors. It needs Chrome or
  `npx playwright install chromium`. Extend `scripts/shots.mjs` for each new screen and flow,
  including the viewer UI (`&role=viewer`).
- Against a real orchestrator only on a throwaway one: clean env, temp state dir AND temp
  routines dir (the default routines dir is the owner's real one), ports of its own, no updates:

  ```sh
  T=$(mktemp -d)
  env -i PATH="$PATH" HOME="$HOME" mis-orchestrator --bind 127.0.0.1:7801 \
    --apps-bind 127.0.0.1:7802 --state-dir "$T/state" --routines-dir "$T/routines" \
    --apps-dev --update-channel off --no-planner --no-summary & P=$!
  sleep 2
  MIS_URL=ws://127.0.0.1:7801 mis apps publish <slug> dist --activate   # throwaway only
  MIS_APPS_URL=http://127.0.0.1:7802 npm run shots
  kill $P
  ```

  Loopback requests are the owner; `MIS_DEV_ROLE=viewer` (with `--apps-dev`) tries other roles.

## PR

- Title and commits in Conventional Commits. Body: what and why, data model (collections, ids,
  fields), test and shots output, the staged release number, screenshots.
- Screenshots at 390 px, light and dark, of every new or changed screen; for a change, before
  (shots from an `origin/main` worktree, renamed `before-*.png`) and after.
- Images go on the mis-notes `pr-media` branch, never a gist and never in the app:

  ```sh
  D=$(mktemp -d)/m; B=$(git branch --show-current)
  git fetch origin pr-media && git worktree add --detach "$D" origin/pr-media
  mkdir -p "$D/$B" && cp shots/*.png "$D/$B/"
  git -C "$D" add "$B" && git -C "$D" commit -m "chore: $B pr screenshots"
  git -C "$D" push origin HEAD:pr-media && git -C "$D" rev-parse HEAD   # the sha for links
  git worktree remove --force "$D"
  ```

  Link each as `https://github.com/<owner>/<repo>/blob/<sha>/<branch>/<file>.png?raw=true`.

## Private data

Real household data (`apps/<slug>/seed/`, screenshots of real records) stays in mis-notes and its
`pr-media` branch. Never put it in a public repo, issue, gist or log. Tests, mock seeds and shots
use invented data.
