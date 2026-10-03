import test from 'node:test';
import assert from 'node:assert/strict';
import { DiffView, MAX_DIFF_LINES, parseUnifiedDiff } from '../diff.js';

test('preserves headers and assigns old/new line numbers to a normal diff', () => {
  const result = parseUnifiedDiff([
    'diff --git a/example.js b/example.js',
    'index 123..456 100644',
    '--- a/example.js',
    '+++ b/example.js',
    '@@ -4,3 +4,3 @@ function example()',
    ' unchanged',
    '-before',
    '+after',
    ' last',
    '',
  ].join('\n'));
  assert.deepEqual(result.lines.map(({ kind, oldLine, newLine }) => [kind, oldLine, newLine]), [
    ['header', null, null], ['meta', null, null],
    ['header', null, null], ['header', null, null], ['hunk', null, null],
    ['context', 4, 4], ['remove', 5, null], ['add', null, 5], ['context', 6, 6],
  ]);
  assert.equal(result.lines[6].text, '-before');
  assert.equal(result.truncated, false);
});

test('resets line numbers across hunks, including omitted counts', () => {
  const { lines } = parseUnifiedDiff('@@ -1 +1 @@\n-a\n+b\n@@ -20,2 +30,2 @@\n same\n-old\n+new');
  assert.deepEqual(lines.filter(line => line.oldLine !== null || line.newLine !== null)
    .map(({ oldLine, newLine }) => [oldLine, newLine]), [
    [1, null], [null, 1], [20, 30], [21, null], [null, 31],
  ]);
});

test('supports empty-side additions and deletions without fake line zeroes', () => {
  const added = parseUnifiedDiff('--- /dev/null\n+++ b/new\n@@ -0,0 +1,2 @@\n+one\n+two');
  assert.deepEqual(added.lines.slice(3).map(line => [line.kind, line.oldLine, line.newLine]), [
    ['add', null, 1], ['add', null, 2],
  ]);
  const removed = parseUnifiedDiff('@@ -1,2 +0,0 @@\n-one\n-two');
  assert.deepEqual(removed.lines.slice(1).map(line => [line.kind, line.oldLine, line.newLine]), [
    ['remove', 1, null], ['remove', 2, null],
  ]);
});

test('distinguishes header-looking content from file headers', () => {
  const { lines } = parseUnifiedDiff('@@ -1 +1 @@\n--- content\n+++ content\n--- a/next\n+++ b/next\n@@ -8 +8 @@\n x');
  assert.deepEqual(lines.map(line => line.kind), [
    'hunk', 'remove', 'add', 'header', 'header', 'hunk', 'context',
  ]);
  assert.equal(lines[6].oldLine, 8);
});

test('normalizes CRLF and retains no-newline markers without consuming line numbers', () => {
  const { lines, truncated } = parseUnifiedDiff([
    '@@ -7,2 +7,2 @@', '-before', '\\ No newline at end of file',
    '+after', '\\ No newline at end of file', ' next', '',
  ].join('\r\n'));
  assert.deepEqual(lines.map(line => [line.kind, line.oldLine, line.newLine]), [
    ['hunk', null, null], ['remove', 7, null], ['no-newline', null, null],
    ['add', null, 7], ['no-newline', null, null], ['context', 8, 8],
  ]);
  assert.equal(lines[2].text, '\\ No newline at end of file');
  assert.equal(lines.some(line => line.text.endsWith('\r')), false);
  assert.equal(truncated, false);
});

test('empty input has no rows and a final newline creates no phantom row', () => {
  assert.deepEqual(parseUnifiedDiff(''), { lines: [], truncated: false });
  assert.deepEqual(parseUnifiedDiff(), { lines: [], truncated: false });
  assert.equal(parseUnifiedDiff('metadata\n').lines.length, 1);
  assert.equal(parseUnifiedDiff('metadata').lines.length, 1);
  assert.equal(parseUnifiedDiff('\n').lines.length, 1);
});

test('bounds parsing at the configured prefix and never exceeds the hard cap', () => {
  const source = Array.from({ length: MAX_DIFF_LINES + 1 }, (_, index) => `line ${index}`).join('\n');
  const result = parseUnifiedDiff(source, { maxLines: MAX_DIFF_LINES * 2 });
  assert.equal(result.lines.length, MAX_DIFF_LINES);
  assert.equal(result.truncated, true);
  assert.equal(result.lines.at(-1).text, `line ${MAX_DIFF_LINES - 1}`);
  assert.equal(parseUnifiedDiff('one\ntwo\n', { maxLines: 2 }).truncated, false);
  assert.equal(parseUnifiedDiff('one\ntwo\nthree', { maxLines: 2 }).truncated, true);
});

function visit(node, predicate) {
  if (Array.isArray(node)) return node.flatMap(child => visit(child, predicate));
  if (!node || typeof node !== 'object') return [];
  return [...(predicate(node) ? [node] : []), ...visit(node.props?.children, predicate)];
}

test('renders hostile markup as a text child, never as executable HTML', () => {
  const payload = '+<img src=x onerror="alert(1)"><script>alert(1)</script>';
  const tree = DiffView({ text: `@@ -0,0 +1 @@\n${payload}`, label: 'Selected file changes' });
  assert.equal(tree.props['aria-label'], 'Selected file changes');
  assert.equal(visit(tree, node => node.props?.class === 'diff-content').at(-1).props.children, payload);
  assert.equal(visit(tree, node => 'dangerouslySetInnerHTML' in (node.props ?? {})).length, 0);
  assert.equal(visit(tree, node => node.type === 'img' || node.type === 'script').length, 0);
});

test('renders empty and truncated notices as accessible text', () => {
  const empty = DiffView({ text: '' });
  assert.equal(visit(empty, node => node.props?.class === 'diff-note')[0].props.children, 'No diff to display.');
  const capped = DiffView({ text: 'context\n'.repeat(MAX_DIFF_LINES + 1) });
  assert.equal(visit(capped, node => node.props?.class === 'diff-line').length, MAX_DIFF_LINES);
  const note = visit(capped, node => node.props?.class === 'diff-note')[0];
  assert.equal(note.props.role, 'note');
  assert.match([note.props.children].flat().join(''), /first 2,000 lines/);
});
