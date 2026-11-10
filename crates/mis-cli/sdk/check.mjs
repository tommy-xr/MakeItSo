// `npm run verify` runs this first: is this folder what the `mis` on PATH writes? Stale fails.
// No `mis` (as in CI), or one too old to tell, skips with a note.
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const dir = fileURLToPath(new URL('.', import.meta.url));
const r = spawnSync('mis', ['apps', 'sdk', '--check', dir], { encoding: 'utf8' });
if (r.error?.code === 'ENOENT') console.log('sdk check skipped: no `mis` on PATH');
// 2: a usage error, from a `mis` older than `apps sdk --check`.
else if (r.status === 2) console.log("sdk check skipped: this `mis` can't check; update it");
else {
  process.stdout.write(r.stdout ?? '');
  process.stderr.write(r.stderr ?? '');
  process.exit(r.status ?? 1);
}
