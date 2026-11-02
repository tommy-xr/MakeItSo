// The page's markdown renderer (index.html, between `md:begin` and `md:end`). Run: node --test crates/mis-orchestrator/tests
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const page = readFileSync(new URL('../static/index.html', import.meta.url), 'utf8');
const block = page.slice(page.indexOf('// md:begin'), page.indexOf('// md:end'));
const md = new Function(`${block}; return md;`)();

// Every tag in the output, e.g. ['p', 'a', 'code'].
const tags = (html) => [...html.matchAll(/<([a-z0-9]+)/g)].map((m) => m[1]);
const ALLOWED = new Set(['p', 'br', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'strong', 'em', 'code', 'pre', 'ul', 'ol', 'li', 'a', 'blockquote', 'hr']);

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
  ]) {
    const html = md(src);
    for (const t of tags(html)) assert.ok(ALLOWED.has(t), `${t} in ${html}`);
    assert.ok(!/<(script|img|svg|iframe|style|b|i|u)\b/.test(html), html);
    // The only attributes: a link's (http/https), a numbered list's start and a code block's language.
    for (const [tag, , attrs] of html.matchAll(/<([a-z0-9]+)([^>]*)>/g)) {
      assert.ok(!attrs || /^ (href="https?:\/\/[^"]*" target="_blank" rel="noopener noreferrer"|start="\d+"|data-lang="[\w+#.-]+")$/.test(attrs), tag);
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
  assert.equal(md('```rust\nlet a = 1 < 2;\n\n  *x*\n```\nafter'),
    '<pre data-lang="rust"><code>let a = 1 &lt; 2;\n\n  *x*</code></pre><p>after</p>');
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
