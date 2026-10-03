import assert from 'node:assert/strict';
import { readFile, stat, readdir } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, 'public');
let localReferences = 0;
for (const name of ['index.html', '404.html']) {
  const html = await readFile(resolve(root, name), 'utf8');
  const allIds = [...html.matchAll(/\bid="([^"]+)"/g)].map(match => match[1]);
  const ids = new Set(allIds);
  assert.equal(ids.size, allIds.length, `${name}: duplicate IDs`);
  assert.match(html, /<html lang="en">/);
  assert.match(html, /name="viewport"/);
  assert.doesNotMatch(html, /\son[a-z]+\s*=/i, `${name}: inline event handler`);
  for (const [, target] of html.matchAll(/\b(?:src|href|aria-controls)="([^"]+)"/g)) {
    if (/^https:\/\//.test(target)) { new URL(target); continue; }
    if (target.startsWith('#')) { assert.ok(ids.has(target.slice(1)), `${name}: missing ${target}`); continue; }
    if (!target.startsWith('/')) { assert.ok(ids.has(target), `${name}: missing control ${target}`); continue; }
    const file = resolve(root, `.${target === '/' ? '/index.html' : target}`);
    assert.ok((await stat(file)).isFile(), `${name}: missing ${target}`);
    localReferences++;
  }
}
const css = await readFile(resolve(root, 'site.css'), 'utf8');
for (const [, target] of css.matchAll(/url\(['"]?(\/[^)'"\s]+)['"]?\)/g)) {
  assert.ok((await stat(resolve(root, `.${target}`))).isFile(), `CSS: missing ${target}`);
  localReferences++;
}
const config = JSON.parse(await readFile(resolve(here, 'wrangler.jsonc'), 'utf8'));
assert.equal(config.assets.directory, './public');
assert.equal(config.assets.not_found_handling, '404-page');
assert.ok(!config.main && !config.account_id, 'Static-only config must not contain a Worker entry point or account ID');
assert.equal(config.workers_dev, false);
assert.deepEqual(config.routes, [
  { pattern: 'switchya.com', custom_domain: true },
  { pattern: 'www.switchya.com', custom_domain: true },
], 'Only the two authorized custom domains may be configured');
const pkg = JSON.parse(await readFile(resolve(here, 'package.json'), 'utf8'));
assert.equal(pkg.devDependencies.wrangler, '4.147.0');
const assets = await readdir(root, { recursive: true, withFileTypes: true });
const files = assets.filter(entry => entry.isFile());
console.log(`Site checks passed: 2 HTML pages, ${localReferences} local references, ${files.length} static files; Wrangler ${pkg.devDependencies.wrangler} pinned.`);
