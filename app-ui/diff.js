import { html } from './vendor/preact-htm.js';

export const MAX_DIFF_LINES = 2000;

/** Parse unified diff text without interpreting its contents as markup. */
export function parseUnifiedDiff(text = '', { maxLines = MAX_DIFF_LINES } = {}) {
  const source = typeof text === 'string' ? text : '';
  const limit = Number.isFinite(maxLines)
    ? Math.max(1, Math.min(MAX_DIFF_LINES, Math.floor(maxLines)))
    : MAX_DIFF_LINES;
  const lines = [];
  let offset = 0;
  let oldLine = 0;
  let newLine = 0;
  let oldRemaining = 0;
  let newRemaining = 0;

  // Read only the displayed prefix instead of splitting an arbitrarily large diff.
  while (offset < source.length && lines.length < limit) {
    const end = source.indexOf('\n', offset);
    let line = source.slice(offset, end === -1 ? source.length : end);
    offset = end === -1 ? source.length : end + 1;
    if (line.endsWith('\r')) line = line.slice(0, -1);

    const row = { kind: 'meta', text: line, oldLine: null, newLine: null };
    const hunk = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@.*$/.exec(line);
    if (hunk) {
      const values = [hunk[1], hunk[3], hunk[2] ?? '1', hunk[4] ?? '1'].map(Number);
      if (values.every(Number.isSafeInteger)) {
        [oldLine, newLine, oldRemaining, newRemaining] = values;
        row.kind = 'hunk';
      }
    } else if (line === '\\ No newline at end of file') {
      row.kind = 'no-newline';
    } else if (line.startsWith('diff --git ')) {
      row.kind = 'header';
      oldRemaining = newRemaining = 0;
    } else if (oldRemaining > 0 || newRemaining > 0) {
      if (line.startsWith('-') && oldRemaining > 0) {
        row.kind = 'remove';
        row.oldLine = oldLine++;
        oldRemaining--;
      } else if (line.startsWith('+') && newRemaining > 0) {
        row.kind = 'add';
        row.newLine = newLine++;
        newRemaining--;
      } else if (line.startsWith(' ') && oldRemaining > 0 && newRemaining > 0) {
        row.kind = 'context';
        row.oldLine = oldLine++;
        row.newLine = newLine++;
        oldRemaining--;
        newRemaining--;
      }
    } else if (line.startsWith('--- ') || line.startsWith('+++ ')) {
      row.kind = 'header';
    }
    lines.push(row);
  }

  return { lines, truncated: offset < source.length };
}

export function DiffView({ text = '', label = 'Code diff' } = {}) {
  const { lines, truncated } = parseUnifiedDiff(text);
  return html`<section class="diff-view" aria-label=${label}>
    ${lines.length ? lines.map((line, index) => html`<div
      class="diff-line" data-kind=${line.kind} key=${index}
    ><span class="diff-number" aria-label=${line.oldLine === null ? 'No old line' : `Old line ${line.oldLine}`}>${line.oldLine ?? ''}</span><span
      class="diff-number" aria-label=${line.newLine === null ? 'No new line' : `New line ${line.newLine}`}
    >${line.newLine ?? ''}</span><code class="diff-content">${line.text}</code></div>`)
      : html`<p class="diff-note">No diff to display.</p>`}
    ${truncated && html`<p class="diff-note" role="note">Showing the first ${MAX_DIFF_LINES.toLocaleString('en-US')} lines. Additional lines are omitted from this preview.</p>`}
  </section>`;
}
