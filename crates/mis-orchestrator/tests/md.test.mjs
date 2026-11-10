// The page's pure renderers (index.html): markdown between `md:begin` and `md:end`, evidence helpers between `ev:begin`
// and `ev:end`, diff helpers between `diff:begin` and `diff:end`, notes helpers between `notes:begin` and `notes:end`,
// the network check between `net:begin` and `net:end`, the app composer between `apps:begin` and `apps:end`, upload kinds
// between `refs:begin` and `refs:end`.
// Run: node --test crates/mis-orchestrator/tests
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const page = readFileSync(new URL('../static/index.html', import.meta.url), 'utf8');
const block = page.slice(page.indexOf('// md:begin'), page.indexOf('// md:end'));
const md = new Function(`${block}; return md;`)();
const evBlock = page.slice(page.indexOf('// ev:begin'), page.indexOf('// ev:end'));
const ev = new Function(`${evBlock}; return { ansi, better, change, fmtChange, tone, deltas, tableMd, resultMd };`)();
const diffBlock = page.slice(page.indexOf('// diff:begin'), page.indexOf('// diff:end'));
const df = new Function(`${diffBlock}; return { patchRows, pathParts, diffOffer, prPrompt, pushPrompt };`)();
const notesBlock = page.slice(page.indexOf('// notes:begin'), page.indexOf('// notes:end'));
const netBlock = page.slice(page.indexOf('// net:begin'), page.indexOf('// net:end'));
const awayStatus = new Function(`${netBlock}; return awayStatus;`)();
const appsBlock = page.slice(page.indexOf('// apps:begin'), page.indexOf('// apps:end'));
const ap = new Function(`${appsBlock}; return { composerFor, appTaskBody, draftNote };`)();
const refsBlock = page.slice(page.indexOf('// refs:begin'), page.indexOf('// refs:end'));
const rf = new Function(`${refsBlock}; return { refKind, docExt, servedName };`)();
const nt = { ...new Function(`${notesBlock}; return { noteFront, noteAuthor, noteRows, noteDestGroups, noteDestLabel, noteSuggested, noteWhy, projectDir, projectActive, projectFiles, projectFind, repoHref };`)(), notePath: new Function(`${block}; return notePath;`)() };

// Every tag in the output, e.g. ['p', 'a', 'code'].
const tags = (html) => [...html.matchAll(/<([a-z0-9]+)/g)].map((m) => m[1]);
const ALLOWED = new Set(['span', 'p', 'br', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'strong', 'em', 'code', 'pre', 'ul', 'ol', 'li', 'a', 'blockquote', 'hr', 'table', 'thead', 'tbody', 'tr', 'th', 'td']);

test('HTML in the text stays text', () => {
  for (const src of [
    '<script>alert(1)</script>',
    '<img src=x onerror="alert(1)">',
    '<a href="javascript:alert(1)">x</a>',
    '- <svg onload=alert(1)>\n  <iframe src=//x>',
    '> <style>*{}</style>',
    '```\n</code></pre><script>x</script>\n```',
    '```"><script>x</script>\nx\n```',
    '```js" onclick="x\nx\n```',
    '`</code><script>x</script>`',
    '# <b onclick=x>t</b>',
    '**<i>x</i>** _<u>y</u>_',
    '| <b>h</b> | x |\n| :-: | --: |\n| <script>x</script> | <img src=x onerror=y> |',
    '| a" style="x | b |\n| :- | - |\n| `</td><script>` | [x](javascript:y) |',
    // Highlighted code: strings and comments end inside a token, never in markup.
    '```rust\nlet s = "</span><script>x</script>"; // </span><img src=x onerror=y>\n/* </code></pre><svg onload=x> */\n```',
    '```py\n"""</span><script>x</script>\n\'\'\'<iframe src=//x>\n```',
    '```sh\necho \'</span><style>*{}</style>\' # <script>x</script>\n```',
    '```diff\n+<script>x</script>\n-</span><img src=x onerror=y>\n@@ <b> @@\n```',
    '```json\n{"</span><script>": "<img src=x onerror=y>", "&lt;": 1}\n```',
  ]) {
    const html = md(src);
    for (const t of tags(html)) assert.ok(ALLOWED.has(t), `${t} in ${html}`);
    assert.ok(!/<(script|img|svg|iframe|style|b|i|u)\b/.test(html), html);
    // The only attributes: a link's (http/https), a numbered list's start, a code block's language, a token's class.
    for (const [tag, , attrs] of html.matchAll(/<([a-z0-9]+)([^>]*)>/g)) {
      assert.ok(!attrs || /^ (href="https?:\/\/[^"]*" target="_blank" rel="noopener noreferrer"|start="\d+"|data-lang="[\w+#.-]+"|style="text-align:(left|center|right)"|class="hl-[a-z]")$/.test(attrs), tag);
    }
  }
});

test('links are http(s) only, and their attributes cannot be broken out of', () => {
  assert.equal(md('[x](javascript:alert(1))'), '<p>[x](javascript:alert(1))</p>');
  assert.equal(md('[x](data:text/html,hi)'), '<p>[x](data:text/html,hi)</p>');
  assert.ok(!md('javascript:alert(1)').includes('<a'));
  assert.equal(md('[x](https://a.co/?q="1"&r=2)'),
    '<p><a href="https://a.co/?q=&quot;1&quot;&amp;r=2" target="_blank" rel="noopener noreferrer">x</a></p>');
  const quoted = md('https://a.co/" onmouseover="alert(1)');
  assert.ok(!/<a [^>]*" onmouseover/.test(quoted), quoted);
  // Code inside a link target never lands in the href.
  assert.ok(!md('[x](https://a.co/`b`)').includes('href="https://a.co/<code>'));
});

test('a NUL in the text cannot forge a set-aside span', () => {
  const html = md('`<b>`\u00000\u0000');
  assert.ok(!html.includes('<b>'), html);
});

test('bare URLs link, without trailing punctuation or emphasis inside', () => {
  assert.equal(md('See https://github.com/o/r/pull/12.'),
    '<p>See <a href="https://github.com/o/r/pull/12" target="_blank" rel="noopener noreferrer">https://github.com/o/r/pull/12</a>.</p>');
  assert.ok(md('https://x.co/a_b_c').includes('href="https://x.co/a_b_c"'));
  assert.ok(!md('(https://x.co/a)').includes('a)"'));
});

test('inline: bold, italic, code; snake_case untouched', () => {
  assert.equal(md('**b** *i* _j_ __k__ `c *d*` snake_case_name'),
    '<p><strong>b</strong> <em>i</em> <em>j</em> <strong>k</strong> <code>c *d*</code> snake_case_name</p>');
});

test('blocks: headings, paragraphs, fences, quotes, rules', () => {
  assert.equal(md('# One\n### Three ###\ntext\nmore\n\nnext'),
    '<h1>One</h1><h3>Three</h3><p>text<br>more</p><p>next</p>');
  assert.equal(md('```text\nlet a = 1 < 2;\n\n  *x*\n```\nafter'),
    '<pre data-lang="text"><code>let a = 1 &lt; 2;\n\n  *x*</code></pre><p>after</p>');
  // Only a word that looks like a language labels a block.
  assert.equal(md('~~~ c++ linenos\nx\n~~~'), '<pre data-lang="c++"><code>x</code></pre>');
  assert.equal(md('```<b>\nx\n```'), '<pre><code>x</code></pre>');
  assert.equal(md('> quoted **b**\n> > deeper\n\nout'),
    '<blockquote><p>quoted <strong>b</strong></p><blockquote><p>deeper</p></blockquote></blockquote><p>out</p>');
  assert.equal(md('a\n\n---\nb'), '<p>a</p><hr><p>b</p>');
  // An unclosed fence runs to the end.
  assert.equal(md('```\nx'), '<pre><code>x</code></pre>');
});

test('lists: bullets, numbers, nesting, start, loose items', () => {
  assert.equal(md('- a\n- b\n  - c\n    1. d\n- e'),
    '<ul><li>a</li><li>b<ul><li>c<ol><li>d</li></ol></li></ul></li><li>e</li></ul>');
  assert.equal(md('3. x\n4. y'), '<ol start="3"><li>x</li><li>y</li></ol>');
  assert.equal(md('- a\n\n- b\n\npara'), '<ul><li>a</li><li>b</li></ul><p>para</p>');
  assert.equal(md('- a\n1. b'), '<ul><li>a</li></ul><ol><li>b</li></ol>');
  assert.equal(md('text\n- item'), '<p>text</p><ul><li>item</li></ul>');
});

test('deep nesting stays bounded', () => {
  for (const src of ['> '.repeat(5000) + 'x', Array.from({ length: 3000 }, (_, i) => `${' '.repeat(i * 2)}- x`).join('\n')]) {
    const html = md(src);
    assert.ok(html.includes('x'));
    for (const t of tags(html)) assert.ok(ALLOWED.has(t), t);
  }
});

test('long URL-like runs render in linear time', () => {
  for (const tail of ['"', '.', ')', '&']) {
    const t0 = Date.now();
    md(`https://${tail.repeat(80000)}`);
    assert.ok(Date.now() - t0 < 500, `${tail}: ${Date.now() - t0} ms`);
  }
});

test('pipe tables: header, alignment, escaped pipes, inline formatting, ragged rows', () => {
  assert.equal(md('| a | b | c |\n| :- | :-: | -: |\n| **x** | `y\\|z` | 1 |\n| only |'),
    '<table><thead><tr><th style="text-align:left">a</th><th style="text-align:center">b</th><th style="text-align:right">c</th></tr></thead>'
    + '<tbody><tr><td style="text-align:left"><strong>x</strong></td><td style="text-align:center"><code>y|z</code></td><td style="text-align:right">1</td></tr>'
    + '<tr><td style="text-align:left">only</td><td style="text-align:center"></td><td style="text-align:right"></td></tr></tbody></table>');
  // Outer pipes are optional; a table can follow a paragraph line and ends at a blank line.
  assert.equal(md('Results:\na | b\n--|--\n1 | 2\n\nafter'),
    '<p>Results:</p><table><thead><tr><th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table><p>after</p>');
  // Not tables: a delimiter row with another cell count, a rule under a line, pipes without a delimiter row.
  assert.ok(!md('| a | b |\n| --- |\n| 1 | 2 |').includes('<table'));
  assert.ok(!md('a\n---').includes('<table'));
  assert.equal(md('a | b\nc | d'), '<p>a | b<br>c | d</p>');
});

test('table rows render in linear time', () => {
  const t0 = Date.now();
  md(`| a |\n|${' '.repeat(50000)}-${' '.repeat(50000)}x`);
  md(`${'|'.repeat(50000)}\n${'|-'.repeat(25000)}`);
  assert.ok(Date.now() - t0 < 500, `${Date.now() - t0} ms`);
});

// A code block's tokens as [class, text], unescaped; plain runs as ['', text]. Spans never nest.
function tokens(src) {
  const html = md(src).replace(/^<pre[^>]*><code>|<\/code><\/pre>$/g, '');
  assert.ok(!/<span[^>]*>[^<]*<span/.test(html), html);
  const raw = { '&lt;': '<', '&gt;': '>', '&amp;': '&', '&quot;': '"', '&#39;': "'" };
  const unesc = (s) => s.replace(/&(?:lt|gt|amp|quot|#39);/g, (e) => raw[e]);
  return [...html.matchAll(/<span class="hl-([a-z])">([^<]*)<\/span>|([^<]+)/g)].map((m) => (m[1] ? [m[1], unesc(m[2])] : ['', unesc(m[3])]));
}
const only = (src, cls) => tokens(src).filter(([c]) => c === cls).map(([, t]) => t);

test('highlight: keywords, strings, comments, numbers, types and calls per language', () => {
  const rust = '```rust\nfn main() {\n    let s: String = "a \\"b\\" < c".into(); // done <ok>\n    println!("{}", 0x1f_u8 + 2.5e3);\n    /* x */ Vec::<i32>::new()\n}\n```';
  assert.deepEqual(only(rust, 'k'), ['fn', 'let']);
  assert.deepEqual(only(rust, 's'), ['"a \\"b\\" < c"', '"{}"']);
  assert.deepEqual(only(rust, 'c'), ['// done <ok>', '/* x */']);
  assert.deepEqual(only(rust, 'n'), ['0x1f_u8', '2.5e3']);
  assert.deepEqual(only(rust, 't'), ['String', 'Vec', 'i32']);
  assert.deepEqual(only(rust, 'f'), ['main', 'into', 'println!', 'new']);
  // Every token joined is the code itself.
  assert.equal(tokens(rust).map(([, t]) => t).join(''), rust.split('\n').slice(1, -1).join('\n'));
  const py = '```python\n@cache\ndef f(x: int) -> None:\n    """doc\n    more"""\n    return x or \'y\'  # why\n```';
  assert.deepEqual(only(py, 'k'), ['def', 'return', 'or']);
  assert.deepEqual(only(py, 's'), ['"""doc\n    more"""', "'y'"]);
  assert.deepEqual(only(py, 'f'), ['@cache', 'f']);
  assert.deepEqual([only(py, 't'), only(py, 'n'), only(py, 'c')], [['int'], ['None'], ['# why']]);
  const ts = '```ts\nconst n: number = await get(`/a/${b}`, null); // c\n```';
  assert.deepEqual([only(ts, 'k'), only(ts, 't'), only(ts, 's'), only(ts, 'n'), only(ts, 'f')],
    [['const', 'await'], ['number'], ['`/a/${b}`'], ['null'], ['get']]);
  const sh = '```bash\nif [ -n "$X" ]; then echo ${HOME} a#b; fi # end\n```';
  assert.deepEqual([only(sh, 'k'), only(sh, 's'), only(sh, 't'), only(sh, 'c')], [['if', 'then', 'fi'], ['"$X"'], ['${HOME}'], ['# end']]);
  const json = '```json\n{"a": -1.5, "b": [true, null, "s"]}\n```';
  assert.deepEqual([only(json, 'p'), only(json, 's'), only(json, 'n')], [['"a"', '"b"'], ['"s"'], ['-1.5', 'true', 'null']]);
  const toml = '```toml\n[package]\nname = "mis" # c\nedition = 2024\nx = [1]\n```';
  assert.deepEqual([only(toml, 't'), only(toml, 'p'), only(toml, 's'), only(toml, 'c'), only(toml, 'n')],
    [['[package]'], ['name', 'edition', 'x'], ['"mis"'], ['# c'], ['2024', '1']]);
  const yaml = '```yml\non: push # c\njobs:\n  - run: cargo test\n    if: true\n```';
  assert.deepEqual([only(yaml, 'p'), only(yaml, 'c'), only(yaml, 'n')], [['on', 'jobs', 'run', 'if'], ['# c'], ['true']]);
  const go = '```go\nfunc main() { var e error = nil; fmt.Println("hi", 42) }\n```';
  assert.deepEqual([only(go, 'k'), only(go, 't'), only(go, 'n'), only(go, 'f')], [['func', 'var'], ['error'], ['nil', '42'], ['main', 'Println']]);
});

test('highlight: diff lines; unknown languages plain; open tokens stay in their block', () => {
  assert.deepEqual(tokens('```diff\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n same\n```'),
    [['m', '--- a/x'], ['', '\n'], ['m', '+++ b/x'], ['', '\n'], ['m', '@@ -1 +1 @@'], ['', '\n'], ['d', '-old'], ['', '\n'], ['a', '+new'], ['', '\n same']]);
  assert.equal(md('```brainfuck\nfn "x" // y\n```'), '<pre data-lang="brainfuck"><code>fn &quot;x&quot; // y</code></pre>');
  assert.equal(md('```\nfn x\n```'), '<pre><code>fn x</code></pre>');
  // An open string ends with its line; an open comment, or a fence still streaming, with the block.
  assert.deepEqual(only('```js\nlet a = "open\nlet b = 1\n```', 'k'), ['let', 'let']);
  assert.deepEqual(only('```rust\nx /* open\nfn y\n```', 'c'), ['/* open\nfn y']);
  assert.deepEqual(only('```python\ndef f(', 'k'), ['def']);
  // Big blocks stay plain; the rest highlight in linear time.
  assert.ok(!md(`\`\`\`rust\n${'let x = "a"; // b\n'.repeat(2000)}\`\`\``).includes('<span'));
  const t = performance.now();
  md(`\`\`\`rust\n${'"'.repeat(19000)}\n\`\`\``);
  md(`\`\`\`js\n${'/*'.repeat(9000)}\n\`\`\``);
  md(`\`\`\`python\n${'\'\'\''.repeat(6000)}\n\`\`\``);
  md(`\`\`\`rust\n${'r#"'.repeat(6000)}\n\`\`\``);
  for (const [lang, run] of [['yaml', '/'], ['yaml', 'a.'], ['yml', '-'], ['toml', '.'], ['toml', 'a.']]) md(`\`\`\`${lang}\n${run.repeat(19000 / run.length)}\n\`\`\``);
  assert.ok(performance.now() - t < 500, `${performance.now() - t} ms`);
});

test('ansi: colour and bold kept, every other escape and control dropped', () => {
  const E = '\x1b';
  assert.deepEqual(ev.ansi(`${E}[1;32mok${E}[0m done ${E}[91mbad${E}[39m\n`), [
    { text: 'ok', fg: 2, bold: true }, { text: ' done ', fg: null, bold: false }, { text: 'bad', fg: 1, bold: false }, { text: '\n', fg: null, bold: false }]);
  // 256-colour: the first 16 map, the rest are plain; RGB is skipped with its arguments.
  assert.deepEqual(ev.ansi(`${E}[38;5;4ma${E}[38;5;200mb${E}[38;2;1;2;3;1mc`).map((s) => [s.text, s.fg, s.bold]), [['a', 4, false], ['b', null, false], ['c', null, true]]);
  // The colon form keeps its arguments to itself; RGB is plain like a 256-colour past 15.
  assert.deepEqual(ev.ansi(`${E}[31m${E}[38:2::1:2:0mx${E}[38:5:2my`).map((s) => [s.text, s.fg, s.bold]), [['x', null, false], ['y', 2, false]]);
  // The worker's transcript stripper's cases (mis-worker transcript::plain) strip the same here.
  const plain = (raw) => ev.ansi(raw).map((s) => s.text).join('');
  for (const [raw, want] of [
    [`${E}[2m2026-11-01T10:00:00Z${E}[0m ${E}[32m INFO${E}[0m ready`, '2026-11-01T10:00:00Z  INFO ready'],
    [`${E}[38;2;153;153;153mdim${E}[39m`, 'dim'],
    [`${E}]8;;https://x.example${E}\\link${E}]8;;${E}\\ ${E}]0;title\x07done`, 'link done'],
    ['\u009b1mbold\u009b0m', 'bold'],
    [`before${E}P1;2|payload${E}\\after ${E}_apc\x07!`, 'beforeafter !'],
    [`cut ${E}Pno end`, 'cut '],
    [`${E}]0;8-bit end\u009ckept`, 'kept'],
    [`${E}=keypad ${E}(Bascii`, 'keypad ascii'],
    [`cut ${E}[38;5`, 'cut '],
    ['plain é text', 'plain é text'],
  ]) assert.equal(plain(raw), want, JSON.stringify(raw));
  // Carriage returns, bells and other controls go too; tabs and newlines stay.
  assert.equal(plain(`a${E}[2K${E}[1Gb\r\x07${E}7c\td\ne`), 'abc\td\ne');
  assert.equal(ev.ansi('\u009b1mx')[0].bold, true);
  // Markup is plain text to it; the page sets segments as text.
  assert.equal(ev.ansi('<script>x</script>')[0].text, '<script>x</script>');
});

test('better: throughput up, time and size down, unknown neutral; most specific text first', () => {
  for (const t of ['ops/s', '820 fps', 'req/s', 'score', 'throughput', '12 qps']) assert.equal(ev.better(t), 'up', t);
  for (const t of ['2.4 ms', '3 s', '12 µs', '412 MB', '1.2 KiB', 'bytes', '35%', 'boot time', 'p99 latency']) assert.equal(ev.better(t), 'down', t);
  for (const t of ['draw calls', '412', 'cases']) assert.equal(ev.better(t), '', t);
  // Rates of success go up even as percentages; other percentages go down.
  for (const t of ['coverage 81%', 'cache hit rate', 'pass rate 99%', 'accuracy']) assert.equal(ev.better(t), 'up', t);
  assert.equal(ev.better('cpu 35%'), 'down');
  assert.equal(ev.better('throughput', 'case ms'), 'up');
  assert.equal(ev.better('draw calls', 'before after', 'Frame (ms)'), 'down');
});

test('deltas: change, format and tone per row; the biggest change', () => {
  assert.equal(ev.fmtChange(-20.83), '−21%');
  assert.equal(ev.fmtChange(1.456), '+1.5%');
  assert.equal(ev.fmtChange(0.01), '0%');
  assert.equal(ev.change(0, 5), null);
  assert.equal(ev.change(null, 5), null);
  assert.equal(ev.tone(-5, 'down'), 'good');
  assert.equal(ev.tone(5, 'down'), 'warn');
  assert.equal(ev.tone(10.4, 'down'), 'bad');
  assert.equal(ev.tone(-12, 'up'), 'bad');
  assert.equal(ev.tone(23, 'up'), 'good');
  assert.equal(ev.tone(23, ''), '');
  const c = (text) => ({ text, ...(/^-?[\d.]+/.test(text) ? { num: parseFloat(text) } : {}) });
  const table = { header: ['case', 'before', 'after'], before: 1, after: 2, rows: [
    ['1080p frame', '2.4 ms', '1.9 ms'], ['throughput', '820 fps', '1010 fps'], ['peak memory', '412 MB', '455 MB'], ['draw calls', '412', '418'], ['new', '—', '3']].map((r) => r.map(c)) };
  const d = ev.deltas(table, 'bench');
  assert.ok(d.has);
  assert.deepEqual(d.rows.map((r) => [r.label, r.delta === null ? null : ev.fmtChange(r.delta), r.tone]),
    [['1080p frame', '−21%', 'good'], ['throughput', '+23%', 'good'], ['peak memory', '+10%', 'bad'], ['draw calls', '+1.5%', ''], ['new', null, '']]);
  assert.equal(d.top.label, 'throughput');
  // Different units: no delta (900 ms → 1.2 s is slower, not 99.9% faster).
  const mixed = ev.deltas({ header: ['case', 'before', 'after'], before: 1, after: 2, rows: [[c('load'), c('900 ms'), c('1.2 s')]] });
  assert.equal(mixed.rows[0].delta, null);
  assert.equal(mixed.top, null);
  // No before/after columns: no deltas.
  assert.equal(ev.deltas({ header: ['a', 'b'], rows: [[c('1'), c('2')]] }).has, false);
});

test('markdown for a PR: table with Δ, escaped cells, output fenced, image placeholders', () => {
  const table = { header: ['case', 'before', 'after'], before: 1, after: 2, rows: [
    [{ text: 'a|b' }, { text: '2 ms', num: 2 }, { text: '1 ms', num: 1 }]] };
  assert.equal(ev.tableMd(table, 't'), '| case | before | after | Δ |\n| --- | ---: | ---: | ---: |\n| a\\|b | 2 ms | 1 ms | −50% |');
  const evidence = [
    { kind: 'compare', title: 'Home page', before: { id: 'b', mime: 'image/png' }, after: { id: 'a', mime: 'image/png' } },
    { kind: 'image', title: 'Demo [gif]', file: { id: 'g', mime: 'image/gif' } },
    { kind: 'table', title: 'Bench', file: { id: 't' } },
    { kind: 'output', title: 'cargo <test>', file: { id: 'o' }, lines: 2, truncated: true },
    { kind: 'output', title: 'not fetched', file: { id: 'x' }, lines: 1, truncated: false }];
  const got = new Map([['t', table], ['o', '\x1b[32m```ok```\x1b[0m\n']]);
  const out = ev.resultMd({ headline: 'Faster', points: ['one', 'two'] }, evidence, got);
  assert.equal(out, [
    '**Faster**', '- one\n- two',
    '**Home page**\n\n| Before | After |\n| --- | --- |\n| ![before](home-page-before.png) | ![after](home-page-after.png) |',
    '![Demo gif](demo-gif.gif)',
    `**Bench**\n\n${ev.tableMd(table, 'Bench')}`,
    '<details><summary>cargo &lt;test&gt; (last 2 lines)</summary>\n\n````\n```ok```\n````\n\n</details>',
    '<!-- Images need uploading: replace the placeholder links above with hosted ones. -->'].join('\n\n') + '\n');
  assert.equal(ev.resultMd(null, [], new Map()), '\n');
});

test('patch rows: hunks numbered, headers dropped, CRs and no-newline notes', () => {
  const patch = 'diff --git a/a b/a\nindex 1..2 100644\n--- a/a\n+++ b/a\n@@ -3,3 +3,4 @@ fn x\n ctx\r\n-old\n+new\r\n+<script>x</script>\n'
    + ' end\n\\ No newline at end of file\n@@ -0,0 +1 @@\n+top\n';
  assert.deepEqual(df.patchRows(patch), [
    { k: 'h', n: null, text: '@@ -3,3 +3,4 @@ fn x' }, { k: 'c', n: 3, text: 'ctx' }, { k: 'd', n: 4, text: 'old' },
    { k: 'a', n: 4, text: 'new' }, { k: 'a', n: 5, text: '<script>x</script>' }, { k: 'c', n: 6, text: 'end' },
    { k: 'm', n: null, text: 'No newline at end of file' }, { k: 'h', n: null, text: '@@ -0,0 +1 @@' }, { k: 'a', n: 1, text: 'top' }]);
  // A file swapped for a symlink: two sections, each one's headers dropped.
  const swap = 'diff --git a/f b/f\ndeleted file mode 100644\n--- a/f\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n'
    + 'diff --git a/f b/f\nnew file mode 120000\n--- /dev/null\n+++ b/f\n@@ -0,0 +1 @@\n+t\n\\ No newline at end of file\n';
  assert.deepEqual(df.patchRows(swap).map((r) => r.k + r.text), ['h@@ -1 +0,0 @@', 'dx', 'h@@ -0,0 +1 @@', 'at', 'mNo newline at end of file']);
  // A rename or mode change without hunks shows its header lines.
  assert.deepEqual(df.patchRows('diff --git a/x b/y\nsimilarity index 100%\nrename from x\nrename to y\n').map((r) => r.text),
    ['similarity index 100%', 'rename from x', 'rename to y']);
});

test('paths split at the last slash; offers follow the branch, remote and thread', () => {
  assert.deepEqual(df.pathParts('crates/a b/é.rs'), ['crates/a b/', 'é.rs']);
  assert.deepEqual(df.pathParts('top.md'), ['', 'top.md']);
  const repo = (r) => ({ branch: 'main', upstream: 'origin/main', ahead: 0, default_branch: 'main', commits: 0, github: true, ...r });
  const offer = (files, r, pr = false) => df.diffOffer({ files, repo: repo(r) }, pr);
  assert.deepEqual(offer(2, {}), { pr: true, push: false });
  assert.deepEqual(offer(2, {}, true), { pr: false, push: false }, 'the thread has a PR');
  assert.deepEqual(offer(2, { github: false }), { pr: false, push: false });
  assert.deepEqual(offer(0, {}), { pr: false, push: false }, 'nothing to propose');
  assert.deepEqual(offer(0, { branch: 'fix-x', upstream: 'origin/fix-x', ahead: 2 }), { pr: true, push: true });
  assert.deepEqual(offer(1, { branch: 'fix-x', upstream: null, commits: 1 }), { pr: true, push: true }, 'never pushed');
  assert.deepEqual(offer(1, { branch: 'fix-x', ahead: 0, commits: 1 }, true), { pr: false, push: false }, 'pushed already');
  assert.deepEqual(offer(1, { branch: 'fix-x', ahead: 0, commits: 1, uncommitted: true }, true), { pr: false, push: true }, 'more edits on the PR');
  assert.deepEqual(offer(1, { branch: 'master', default_branch: null, upstream: null, commits: 1 }), { pr: true, push: false });
  assert.deepEqual(offer(1, { branch: null, upstream: null, commits: 1 }), { pr: true, push: false }, 'detached');
  const pr = df.prPrompt(['notes.md', 'a`b\nc']);
  for (const want of ['gh pr create', 'Conventional Commits', 'report_evidence', 'new branch', 'before/after', '"notes.md", "a`b\\nc"']) assert.ok(pr.includes(want), want);
  assert.ok(!df.prPrompt([]).includes('leave them out'));
  assert.ok(df.pushPrompt([]).includes('push the current branch'));
});

test('a note links to notes and shows its images, relative to its folder, and nothing else', () => {
  const html = md('See [the plan](../projects/amf/README.md) and [top](#x).\n\n![a "sky"](a%20b-1.jpg)', 'inbox');
  assert.ok(html.includes('<a href="#notes/projects/amf/README.md">the plan</a>'), html);
  assert.ok(html.includes('[top](#x)'), html);
  assert.ok(html.includes('<img alt="a &quot;sky&quot;" data-note="inbox%2Fa%20b-1.jpg">'), html);
  // Without a folder, transcripts get neither.
  assert.ok(!md('[x](a.md) ![y](b.png)').includes('<a') && !md('![y](b.png)').includes('<img'));
  // http(s) links still open outside; other schemes and escapes stay text.
  assert.ok(md('[x](https://a.co/)', 'inbox').includes('href="https://a.co/" target="_blank"'));
  for (const src of ['[x](javascript:alert(1))', '![x](data:image/png;base64,AA)', '[x](/etc/passwd)', '[x](../../etc/passwd)',
    '![x](..)', '[x](a\\b.md)', '![x](%zz.png)']) {
    const out = md(src, 'inbox');
    assert.ok(!/<(a|img)\b/.test(out), `${src} -> ${out}`);
  }
  // Nothing in a target or alt breaks out of its attribute.
  for (const src of ['![x" onerror="y](a.png)', '[x](a.md"onclick="y)', '![`</code>"<b>`](a.png)', '[x](a.md?q="><script>)']) {
    const out = md(src, 'ai/projects');
    for (const [, tag, attrs] of out.matchAll(/<([a-z0-9]+)([^>]*)>/g)) {
      assert.ok(!attrs || /^ (href="#notes\/[\w%.~*'()!/-]*"|alt="[^"<>]*" data-note="[\w%.~*'()!-]*")$/.test(attrs), `${tag}${attrs} in ${out}`);
    }
  }
  assert.equal(nt.notePath('inbox', '../projects/a%20b.md'), 'projects/a b.md');
  assert.equal(nt.notePath('ai/projects/amf', '../../../inbox/x.md#top'), 'inbox/x.md');
  assert.equal(nt.notePath('inbox', '../..'), null);
  assert.equal(nt.notePath('inbox', './'), 'inbox');
});

test('a note’s frontmatter, author and folder order', () => {
  const capture = '---\nschema: 1\nid: 0b8a\ncaptured: 2026-11-07T18:42:10Z\nvia: "shortcut"\nurl: "https://a.co/?q=\\"1\\""\nproject: "amf"\n'
    + 'assets:\n  - path: 2026-11-07-a-1.jpg\n    sha256: h\n  - path: 2026-11-07-a-2.png\n    sha256: h\n---\nbody\n---\nmore';
  const { meta, body } = nt.noteFront(capture);
  assert.deepEqual(meta.assets, ['2026-11-07-a-1.jpg', '2026-11-07-a-2.png']);
  assert.equal(meta.via, 'shortcut');
  assert.equal(meta.url, 'https://a.co/?q="1"');
  assert.equal(meta.captured, '2026-11-07T18:42:10Z');
  assert.equal(body, 'body\n---\nmore');
  const record = nt.noteFront("---\nid: all-must-fall\ntitle: 'All Must Fall'\naliases: [amf, 'a m f']\nrepos: [tommy-xr/MakeItSo]   # optional\ntags:\n  - weekly\n---\nText.");
  assert.deepEqual(record.meta, { id: 'all-must-fall', title: 'All Must Fall', aliases: ['amf', 'a m f'], repos: ['tommy-xr/MakeItSo'], tags: ['weekly'] });
  assert.deepEqual(nt.noteFront('# no frontmatter\n---\n'), { meta: {}, body: '# no frontmatter\n---\n' });
  assert.deepEqual(['ai/x.md', 'inbox/x.md', 'projects/x.md', 'ai'].map(nt.noteAuthor), ['AI', 'Capture', 'You', 'You']);
  const e = (path, kind = 'md') => ({ path, kind, size: 1 });
  const entries = [e('inbox', 'dir'), e('inbox/2026-11-06-a.md'), e('inbox/2026-11-08-b.md'), e('inbox/2026-11-08-b-1.jpg', 'image'),
    e('inbox/z', 'dir'), e('inbox/old', 'dir'), e('inbox/old/2026-01-01.md'), e('inboxes/x.md')];
  assert.deepEqual(nt.noteRows(entries, 'inbox').map((x) => x.path),
    ['inbox/old', 'inbox/z', 'inbox/2026-11-08-b.md', 'inbox/2026-11-08-b-1.jpg', 'inbox/2026-11-06-a.md']);
});

test('Move to… groups the server’s folders, projects by title', () => {
  const dests = ['archive', 'areas/home', 'projects/garden', 'projects/mis', 'projects/zed', 'resources/rust'];
  const list = [{ id: 'mis', title: 'Make It So', archived: false }, { id: 'garden', title: 'Garden', archived: false }, { id: 'zed', title: 'Old', archived: true }];
  assert.deepEqual(nt.noteDestGroups(dests, list), [
    ['Projects', [['projects/garden', 'Garden'], ['projects/mis', 'Make It So'], ['projects/zed', 'zed']]],
    ['Areas', [['areas/home', 'home']]], ['Resources', [['resources/rust', 'rust']]], ['Archive', [['archive', 'Archive']]]]);
  assert.deepEqual(nt.noteDestGroups(['projects/mis'], null), [['Projects', [['projects/mis', 'mis']]]], 'titles not in yet; no archive offered');
  assert.deepEqual(nt.noteDestGroups([], []), []);
});

test('an inbox row files as suggested, waits while one is pending, or offers Move to…', () => {
  const dests = ['archive', 'areas/home', 'projects/mis'], list = [{ id: 'mis', title: 'Make It So', archived: false }];
  assert.equal(nt.noteDestLabel('projects/mis', dests, list), 'Make It So');
  assert.equal(nt.noteDestLabel('areas/home', dests, list), 'Areas › home');
  assert.equal(nt.noteDestLabel('archive', dests, list), 'Archive');
  assert.equal(nt.noteDestLabel('areas/gone', dests, list), 'areas/gone');
  const s = (dest, error) => ({ hash: 'h', dest, summary: 'x', at: 1, ...(error ? { error } : {}) });
  assert.equal(nt.noteSuggested(s('projects/mis'), dests, false), 'file');
  assert.equal(nt.noteSuggested(undefined, dests, true), 'wait', 'being asked for');
  assert.equal(nt.noteSuggested(undefined, dests, false), 'none', 'not asked for: suggestions off, or till the next pass');
  assert.equal(nt.noteSuggested(s('areas/gone'), dests, true), 'wait', 'its folder is gone: asked again');
  assert.equal(nt.noteSuggested(s('areas/gone'), dests, false), 'none');
  assert.equal(nt.noteSuggested(s('none'), dests, false), 'none');
  assert.equal(nt.noteSuggested(s('none', 'resources/reading is not a folder to file in'), dests, false), 'none');
  assert.equal(nt.noteWhy('inbox/a.md', 'inbox/a.md changed since it was read'), 'Changed since it was read');
  assert.equal(nt.noteWhy('inbox/a.md', 'offline; try again when connected'), 'Offline; try again when connected');
});

test('a project’s folder, files, name and repo links', () => {
  const mis = { id: 'make-it-so', aliases: ['MIS'], archived: false, status: 'active' }, old = { id: 'old', aliases: [], archived: true, status: 'done' };
  assert.equal(nt.projectDir(mis), 'projects/make-it-so');
  assert.equal(nt.projectDir(old), 'archive/old');
  assert.deepEqual([mis, old, { ...mis, status: 'paused' }].map(nt.projectActive), [true, false, false]);
  assert.equal(nt.projectFind([old, mis], 'mis'), mis);
  assert.equal(nt.projectFind([old, mis], 'Make-It-So'), mis);
  assert.equal(nt.projectFind([old, mis], 'OLD'), old);
  assert.equal(nt.projectFind([old, mis], 'nope'), null);
  const e = (path, kind = 'md') => ({ path, kind, size: 1 });
  const entries = [e('projects/make-it-so', 'dir'), e('projects/make-it-so/README.md'), e('projects/make-it-so/plan.md'),
    e('projects/make-it-so/docs', 'dir'), e('projects/make-it-so/docs/a.md'), e('projects/make-it-sooner/x.md'),
    e('ai/projects/make-it-so', 'dir'), e('ai/projects/make-it-so/2026-11-09-review.md'), e('archive/old/README.md')];
  const files = nt.projectFiles(entries, mis);
  assert.deepEqual(files.own.map((x) => x.path), ['projects/make-it-so/docs', 'projects/make-it-so/plan.md']);
  assert.deepEqual(files.ai.map((x) => x.path), ['ai/projects/make-it-so/2026-11-09-review.md']);
  assert.deepEqual(nt.projectFiles(entries, old), { own: [], ai: [] });
  assert.equal(nt.repoHref('tommy-xr/MakeItSo'), 'https://github.com/tommy-xr/MakeItSo');
  for (const bad of ['MakeItSo', 'a/b/c', '../x', 'a/..', 'javascript:alert(1)', 'a/b?x', ' a/b', 'https://x.co/a']) {
    assert.equal(nt.repoHref(bad), null, bad);
  }
});

test('only a proxy page counts as the orchestrator being away', () => {
  // Cloudflare's 502/530 while the orchestrator restarts.
  assert.equal(awayStatus(502, 'text/html; charset=UTF-8'), true);
  assert.equal(awayStatus(530, 'text/html'), true);
  // The orchestrator's own errors (plain text) are real answers, shown.
  assert.equal(awayStatus(503, 'text/plain; charset=utf-8'), false);
  assert.equal(awayStatus(504, null), false);
  assert.equal(awayStatus(404, 'text/html'), false);
  assert.equal(awayStatus(200, 'application/json'), false);
});

test('the composer builds or changes an app on the apps view, each with its own draft', () => {
  assert.equal(ap.composerFor('list', null).key, 'draft:new');
  assert.equal(ap.composerFor('apps', null).placeholder, 'Start a task or ask anything');
  const made = ap.composerFor('build', null);
  assert.equal(made.key, 'draft:app');
  assert.match(made.placeholder, /^Create an app: what should it do\?/);
  assert.deepEqual(ap.composerFor('build', 'pool', 'Pool'), { key: 'draft:app:pool', placeholder: 'Change Pool: what should change?', label: 'change Pool' });
  assert.equal(ap.composerFor('build', 'pool').placeholder, 'Change pool: what should change?');
});

test('an app task runs where the chips say, else on Settings\' apps harness, with its images', () => {
  const apps = { harness: 'codex', model: 'gpt-5-codex', repo: '~/apps' };
  const img = '00000000-0000-4000-8000-000000000001';
  assert.deepEqual(ap.appTaskBody('Track chlorine.', null, { worker: null, harness: '', model: '' }, apps, [img]),
    { request: 'Track chlorine.', slug: null, change: false, harness: 'codex', model: null, worker: null, images: [img] });
  assert.deepEqual(ap.appTaskBody('Add pH.', 'pool', { worker: { id: 'w1' }, harness: 'claude', model: 'opus' }, apps, []),
    { request: 'Add pH.', slug: 'pool', change: true, harness: 'claude', model: 'opus', worker: 'w1', images: [] });
  assert.equal(ap.appTaskBody('x', null, { worker: null, harness: '', model: '' }, null, []).harness, 'claude'); // Settings unread
});

test('beside a draft, the composer sends in-app feedback on it', () => {
  assert.deepEqual(ap.composerFor('build', 'pool', 'Pool', 2), { key: 'draft:app:pool', placeholder: 'What should change in draft v2?', label: 'feedback on Pool v2' });
  assert.deepEqual(ap.draftNote('Bars too thin.', 358, 600),
    { route: '/', selector: '', rect: { x: 0, y: 0, w: 358, h: 600 }, viewport: { w: 358, h: 600 }, note: 'Bars too thin.' });
});

test('images go by type and documents by the extensions the server takes', () => {
  assert.equal(rf.refKind({ type: 'image/heic', name: 'IMG_1.HEIC' }), 'image');
  assert.equal(rf.refKind({ type: '', name: 'Rules.MD' }), 'doc');
  assert.equal(rf.refKind({ type: 'application/pdf', name: 'Chore list.pdf' }), 'doc');
  for (const name of ['page.html', 'x.svg', 'pdf', 'archive.zip', '']) assert.equal(rf.refKind({ type: 'text/html', name }), null, name);
  assert.equal(rf.docExt('a.tar.JSON'), 'json');
});

test('a served upload\'s name comes from its Content-Disposition', () => {
  assert.equal(rf.servedName("inline; filename*=UTF-8''Chore%20list.pdf"), 'Chore list.pdf');
  assert.equal(rf.servedName("inline; filename*=UTF-8''%E2%9C%93.md"), '✓.md');
  assert.equal(rf.servedName("inline; filename*=UTF-8''%E2"), '');
  assert.equal(rf.servedName(null), '');
});
