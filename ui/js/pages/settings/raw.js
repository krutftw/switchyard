// Settings, Raw file tab: switchyard.toml as text.
//
//   GET  /config/raw       the file as it is on disk (secrets included)
//   POST /config/validate  check a text without saving it
//   PUT  /config/raw       validate, write and apply
//   POST /reload           read the file again and apply it
//
// The file holds every secret in full, so nothing is fetched until the user
// asks to see it.

import { html, useEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import { Button, EmptyState, ErrorState, Form, Modal, Notice, Panel, Skeleton, toast } from '../../components/index.js';
import { api } from '../../lib/api.js';
import { formatDateTime, plural } from '../../lib/format.js';
import { useAsync, useResource, useUid } from '../../lib/hooks.js';
import { liveState, useLive } from '../../lib/live.js';
import { useStore } from '../../lib/store.js';
import { SaveBar, confirmDiscard, expectSecret, secretSettled, sentence, useSaveHotkey, useUnsavedGuard } from './common.js';

// Asked once per page load, not once per visit to the tab.
let revealed = false;

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/** A textarea always reports "\n"; the file may use "\r\n". Edit in one, save in the other. */
const toEditor = (text) => text.replace(/\r\n/g, '\n');
const toFile = (text, eol) => (eol === '\r\n' ? text.replace(/\n/g, '\r\n') : text);

const KEY_PART = '(?:"[^"]*"|\'[^\']*\'|[A-Za-z0-9_-]+)';
const KEY_LINE = new RegExp(`^(${KEY_PART}(?:\\s*\\.\\s*${KEY_PART})*)\\s*=\\s*(.*)$`);

/** `a."b.c".d` -> ['a', 'b.c', 'd'] */
function splitKey(text) {
  const parts = [];
  const pattern = new RegExp(KEY_PART, 'g');
  let match;
  while ((match = pattern.exec(text)) !== null) parts.push(match[0].replace(/^["']|["']$/g, ''));
  return parts;
}

/**
 * Where things are in a TOML text: a map from configuration path
 * ("providers.1.base_url", "routing.cooldown") to the 1-based line that
 * defines it, plus the raw value text of every key. It reads table headers
 * and `key = value` lines, which is all the gateway's issues point at; it
 * is not a TOML parser.
 */
export function indexToml(text) {
  const lines = new Map();
  const values = new Map();
  const counts = new Map(); // array-of-tables path -> entries seen so far
  let table = [];
  let openString = null;

  // Inside an array of tables, a parent path means its latest entry.
  const resolve = (keys) => {
    const out = [];
    for (const key of keys) {
      out.push(key);
      const seen = counts.get(out.join('.'));
      if (seen) out.push(String(seen - 1));
    }
    return out;
  };

  text.split('\n').forEach((raw, i) => {
    const line = raw.trim();
    if (openString) {
      if (line.includes(openString)) openString = null;
      return;
    }
    if (!line || line.startsWith('#')) return;
    let match;
    if ((match = /^\[\[\s*([^\]]+?)\s*\]\]/.exec(line))) {
      const keys = splitKey(match[1]);
      const path = [...resolve(keys.slice(0, -1)), keys[keys.length - 1]];
      const name = path.join('.');
      const seen = counts.get(name) ?? 0;
      counts.set(name, seen + 1);
      table = [...path, String(seen)];
      if (!lines.has(name)) lines.set(name, i + 1);
      lines.set(table.join('.'), i + 1);
      return;
    }
    if ((match = /^\[\s*([^\]]+?)\s*\]/.exec(line))) {
      table = resolve(splitKey(match[1]));
      lines.set(table.join('.'), i + 1);
      return;
    }
    if ((match = KEY_LINE.exec(line))) {
      const path = [...table, ...splitKey(match[1])].join('.');
      if (!lines.has(path)) {
        lines.set(path, i + 1);
        values.set(path, match[2]);
      }
      for (const quote of ['"""', "'''"]) {
        const at = match[2].indexOf(quote);
        if (at !== -1 && match[2].indexOf(quote, at + 3) === -1) openString = quote;
      }
    }
  });
  return { lines, values };
}

/**
 * The line an issue belongs to. Syntax errors carry "line L, column C";
 * configuration issues carry a path, which is looked up in the text (the
 * nearest enclosing table when the field itself is not written out).
 */
export function locateIssue(issue, index) {
  const at = /^line (\d+)(?:, column (\d+))?/.exec(issue.path ?? '');
  if (at) return { line: Number(at[1]), column: at[2] ? Number(at[2]) : 1 };
  const keys = String(issue.path ?? '')
    .replace(/\[(\w+)\]/g, '.$1')
    .split('.')
    .filter(Boolean);
  while (keys.length > 0) {
    const line = index.lines.get(keys.join('.'));
    if (line) return { line, column: 1 };
    keys.pop();
  }
  return null;
}

/** The admin secret as written in the text: a string, or undefined when it is not a plain `secret = "..."`. */
function adminSecretIn(text) {
  const raw = indexToml(text).values.get('admin.secret');
  if (raw == null) return undefined;
  const basic = /^"((?:[^"\\]|\\.)*)"/.exec(raw);
  if (basic) {
    try {
      return JSON.parse(`"${basic[1]}"`);
    } catch {
      return basic[1];
    }
  }
  const literal = /^'([^']*)'/.exec(raw);
  return literal ? literal[1] : undefined;
}

const isReference = (secret) => /^env:/.test(secret) || /^\$\{[^}]*\}$/.test(secret);

// ---------------------------------------------------------------------------
// Line diff
// ---------------------------------------------------------------------------

const DIFF_CELLS = 4_000_000;

/**
 * Line diff of two texts as a list of { kind: 'same' | 'del' | 'add', text,
 * a, b } with 1-based line numbers in the old (a) and new (b) text.
 */
export function diffLines(before, after) {
  const a = before.split('\n');
  const b = after.split('\n');
  let head = 0;
  while (head < a.length && head < b.length && a[head] === b[head]) head += 1;
  let tail = 0;
  while (tail < a.length - head && tail < b.length - head && a[a.length - 1 - tail] === b[b.length - 1 - tail]) tail += 1;

  const ops = [];
  for (let i = 0; i < head; i += 1) ops.push({ kind: 'same', text: a[i], a: i + 1, b: i + 1 });

  const n = a.length - head - tail;
  const m = b.length - head - tail;
  if (n * m > DIFF_CELLS) {
    // Too much changed to align line by line: show it as one replaced block.
    for (let i = 0; i < n; i += 1) ops.push({ kind: 'del', text: a[head + i], a: head + i + 1, b: null });
    for (let j = 0; j < m; j += 1) ops.push({ kind: 'add', text: b[head + j], a: null, b: head + j + 1 });
  } else {
    // Longest common subsequence over the part that differs.
    const width = m + 1;
    const table = new Uint32Array((n + 1) * width);
    for (let i = n - 1; i >= 0; i -= 1) {
      for (let j = m - 1; j >= 0; j -= 1) {
        table[i * width + j] = a[head + i] === b[head + j] ? table[(i + 1) * width + j + 1] + 1 : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    while (i < n || j < m) {
      if (i < n && j < m && a[head + i] === b[head + j]) {
        ops.push({ kind: 'same', text: a[head + i], a: head + i + 1, b: head + j + 1 });
        i += 1;
        j += 1;
      } else if (i < n && (j === m || table[(i + 1) * width + j] >= table[i * width + j + 1])) {
        ops.push({ kind: 'del', text: a[head + i], a: head + i + 1, b: null });
        i += 1;
      } else {
        ops.push({ kind: 'add', text: b[head + j], a: null, b: head + j + 1 });
        j += 1;
      }
    }
  }

  for (let k = 0; k < tail; k += 1) {
    const ai = a.length - tail + k;
    const bi = b.length - tail + k;
    ops.push({ kind: 'same', text: a[ai], a: ai + 1, b: bi + 1 });
  }
  return ops;
}

/** Keep the changed lines and `context` lines around them; the rest becomes { kind: 'gap', count }. */
export function diffHunks(ops, context = 2) {
  const keep = new Uint8Array(ops.length);
  ops.forEach((op, index) => {
    if (op.kind === 'same') return;
    for (let k = Math.max(0, index - context); k <= Math.min(ops.length - 1, index + context); k += 1) keep[k] = 1;
  });
  const rows = [];
  let skipped = 0;
  ops.forEach((op, index) => {
    if (keep[index]) {
      if (skipped > 0) rows.push({ kind: 'gap', count: skipped });
      skipped = 0;
      rows.push(op);
    } else skipped += 1;
  });
  if (skipped > 0 && rows.length > 0) rows.push({ kind: 'gap', count: skipped });
  return rows;
}

const SIGN = { add: '+', del: '−', same: '' };
const SIGN_WORD = { add: 'Added', del: 'Removed' };

function Diff({ before, after }) {
  const { rows, added, removed } = useMemo(() => {
    const ops = diffLines(before, after);
    return {
      rows: diffHunks(ops),
      added: ops.filter((op) => op.kind === 'add').length,
      removed: ops.filter((op) => op.kind === 'del').length,
    };
  }, [before, after]);
  return html`
    <div class="stack" style="--gap:var(--space-2)">
      <p class="muted">${plural(added, 'line')} added, ${plural(removed, 'line')} removed.</p>
      <div class="settings-diff" tabindex="0" role="group" aria-label="Changed lines">
        <table class="settings-diff-table">
          <tbody>
            ${rows.map((row, index) =>
              row.kind === 'gap'
                ? html`<tr class="settings-diff-gap" key=${index}><td colspan="4">${plural(row.count, 'unchanged line')}</td></tr>`
                : html`
                    <tr key=${index} data-kind=${row.kind}>
                      <td class="settings-diff-ln">${row.a ?? ''}</td>
                      <td class="settings-diff-ln">${row.b ?? ''}</td>
                      <td class="settings-diff-sign"><span aria-hidden="true">${SIGN[row.kind]}</span>${SIGN_WORD[row.kind] && html`<span class="sr-only">${SIGN_WORD[row.kind]}</span>`}</td>
                      <td class="settings-diff-text">${row.text === '' ? ' ' : row.text}</td>
                    </tr>
                  `,
            )}
          </tbody>
        </table>
      </div>
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

function countLines(text) {
  let count = 1;
  for (let i = text.indexOf('\n'); i !== -1; i = text.indexOf('\n', i + 1)) count += 1;
  return count;
}

/** A textarea with a line-number gutter. `marks` is a Set of line numbers to flag. */
function TomlEditor({ value, onChange, marks, areaRef, onCursor, invalid }) {
  const gutter = useRef(null);
  const lineCount = useMemo(() => countLines(value), [value]);
  const numbers = useMemo(
    () => Array.from({ length: lineCount }, (_, i) => html`<div key=${i} class="settings-editor-ln" data-mark=${marks.has(i + 1) ? '' : undefined}>${i + 1}</div>`),
    [lineCount, marks],
  );
  const cursor = (event) => {
    const el = event.currentTarget;
    const upTo = el.value.slice(0, el.selectionStart);
    const line = countLines(upTo);
    onCursor({ line, column: upTo.length - upTo.lastIndexOf('\n') });
  };
  return html`
    <div class="settings-editor" data-invalid=${invalid ? '' : undefined}>
      <div class="settings-editor-gutter" ref=${gutter} aria-hidden="true">${numbers}</div>
      <textarea
        ref=${areaRef}
        class="settings-editor-area"
        wrap="off"
        spellcheck=${false}
        autocapitalize="off"
        autocomplete="off"
        autocorrect="off"
        aria-label="Contents of switchyard.toml"
        value=${value}
        onInput=${(event) => onChange(event.currentTarget.value)}
        onScroll=${(event) => {
          if (gutter.current) gutter.current.scrollTop = event.currentTarget.scrollTop;
        }}
        onSelect=${cursor}
        onKeyUp=${cursor}
        onClick=${cursor}
      ></textarea>
    </div>
  `;
}

/** The outcome of a validation, a refused save or a refused reload. */
function Report({ report, stale, onJump }) {
  if (!report) return null;
  if (report.ok) {
    return html`<${Notice} tone="clear" title="The file is valid">${stale ? 'That was before your last edit. Validate again to check the text as it is now.' : 'Nothing was saved. Save to write it to disk and apply it.'}<//>`;
  }
  return html`
    <${Notice} tone="stop" title=${report.title}>
      ${report.message && html`<span>${report.message}</span>`}
      ${stale && html`<span>${report.disk ? 'The line numbers are those of the file on disk, which is not the text in the editor.' : ' The text changed since this check, so line numbers may be off. Validate again.'}</span>`}
      ${report.issues.length > 0 &&
      html`<ul class="settings-issues">
        ${report.issues.map(
          (issue, i) => html`
            <li key=${i}>
              ${issue.at
                ? html`<button type="button" class="settings-issue-jump" onClick=${() => onJump(issue.at.line, issue.at.column)}>Line ${issue.at.line}</button>`
                : html`<span class="settings-issue-noline">No line</span>`}
              <span class="settings-issue-text">
                ${issue.path && !/^line \d+/.test(issue.path) && html`<span class="issue-path">${issue.path}</span>`}
                <span>${issue.message}</span>
              </span>
            </li>
          `,
        )}
      </ul>`}
    <//>
  `;
}

/** Issues with the line each one belongs to in `text` (null when it cannot be found). */
function annotate(issues, text) {
  const index = indexToml(text);
  return (issues ?? []).map((issue) => ({ ...issue, at: locateIssue(issue, index) }));
}

// How often the file is read again while the gateway refuses it.
const REFUSED_POLL_MS = 5000;

/**
 * onConfig  called with the new configuration view after a save or reload
 * refused   the refusal, while the gateway refuses the file that is on disk
 *           (the config.reloaded frame, a new object for each one), else null
 * onValid   called with that refusal when the file on disk is valid again
 */
function RawEditor({ onConfig, refused = null, onValid }) {
  const liveOpen = useStore(liveState, (s) => s.status === 'open');
  const file = useResource('/config/raw', { pollMs: liveOpen ? 0 : 20_000 });
  useLive('config.reloaded', () => secretSettled().then(file.refresh));

  const formId = useUid('raw-form');
  const area = useRef(null);
  const [text, setText] = useState(null); // what is in the editor
  const [base, setBase] = useState(null); // the disk version the edits started from
  const [conflict, setConflict] = useState(false);
  const [report, setReport] = useState(null);
  const [review, setReview] = useState(false);
  const [cursor, setCursor] = useState({ line: 1, column: 1 });
  const reviewed = useRef(null);

  const diskRaw = file.data?.text;
  const disk = diskRaw == null ? null : toEditor(diskRaw);
  const eol = diskRaw != null && diskRaw.includes('\r\n') ? '\r\n' : '\n';
  const dirty = text !== null && text !== base;

  // The file on disk changed (or arrived for the first time).
  useEffect(() => {
    if (disk === null || disk === base) return;
    if (text === null || text === base) {
      setText(disk);
      setBase(disk);
      setConflict(false);
    } else if (disk === text) {
      setBase(disk);
      setConflict(false);
    } else setConflict(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [disk]);

  useUnsavedGuard(dirty, 'switchyard.toml');
  useSaveHotkey(formId, dirty && !review);

  const validate = useAsync((body) => api.post('/config/validate', { text: body }));
  const save = useAsync((body) => api.put('/config/raw', { text: body }));
  const reload = useAsync(() => api.post('/reload'));

  // A refused save: put the cursor on the first line the gateway named.
  const jumpRef = useRef(null);
  useEffect(() => {
    if (!save.error || text === null) return undefined;
    const first = (save.error.issues ?? []).map((issue) => locateIssue(issue, indexToml(text))).find(Boolean);
    if (!first) return undefined;
    // After the review dialog has handed the focus back.
    const timer = setTimeout(() => jumpRef.current?.(first.line, first.column), 60);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [save.error]);

  // The file on disk was refused: say which lines, and notice when it is
  // valid again. The gateway sends no frame when the file is put back the
  // way it was, so it is read again every few seconds until it validates.
  //
  // Only a text read after the refusal is judged. The text that was already
  // loaded is the one from before the breaking edit: it validates, and
  // taking that as "valid again" would clear the notice just as the broken
  // file arrives in the editor.
  const [diskIssues, setDiskIssues] = useState(null);
  const onValidRef = useRef(onValid);
  onValidRef.current = onValid;
  const mutateFile = file.mutate;
  useEffect(() => {
    if (!refused) {
      setDiskIssues(null);
      return undefined;
    }
    let alive = true;
    let timer = null;
    let judged = null; // the text of the last verdict
    const look = async () => {
      try {
        await secretSettled();
        const fresh = await api.get('/config/raw');
        if (!alive) return;
        mutateFile(fresh);
        if (fresh.text !== judged) {
          const verdict = await api.post('/config/validate', { text: fresh.text });
          if (!alive) return;
          judged = fresh.text;
          if (verdict.ok) {
            setDiskIssues(null);
            onValidRef.current?.(refused);
            return;
          }
          setDiskIssues({ issues: verdict.issues, forText: toEditor(fresh.text) });
        }
      } catch {
        // The gateway did not answer: the next round asks again.
      }
      if (alive) timer = setTimeout(look, REFUSED_POLL_MS);
    };
    look();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refused, mutateFile]);

  // What is reported under the editor, most recent cause first: a refused
  // save or reload or a check that could not run, the last validation, the
  // problems of the file on disk.
  const failure = save.error ?? reload.error ?? validate.error;
  let shown = report;
  if (failure) {
    const title = save.error ? 'Not saved: the gateway refused the file' : reload.error ? 'The file on disk was refused' : 'Could not validate the file';
    const tail = save.error ? ' Nothing was written.' : reload.error ? ' The previous configuration stays in effect.' : '';
    shown = { ok: false, title, message: `${sentence(failure.message)}${tail}`, issues: annotate(failure.issues, text ?? ''), forText: text };
  } else if (!report && diskIssues) {
    shown = { ok: false, disk: true, title: 'Problems in the file on disk', issues: annotate(diskIssues.issues, diskIssues.forText), forText: diskIssues.forText };
  }
  const shownStale = shown != null && shown.forText !== text;
  const markKey = shown && !shown.ok && !shownStale ? shown.issues.flatMap((issue) => (issue.at ? [issue.at.line] : [])).join(',') : '';
  const marks = useMemo(() => new Set(markKey ? markKey.split(',').map(Number) : []), [markKey]);

  if (file.error && text === null) {
    return html`<${Panel} flush><${ErrorState} title="Could not read switchyard.toml" error=${file.error} onRetry=${file.refresh} /><//>`;
  }
  if (text === null) {
    return html`<${Panel} title="switchyard.toml" aria-busy="true" aria-label="Loading the file"><${Skeleton} lines=${12} /><//>`;
  }

  const jump = (line, column = 1) => {
    const el = area.current;
    if (!el) return;
    const lines = text.split('\n');
    const target = Math.min(Math.max(1, line), lines.length);
    let start = 0;
    for (let i = 0; i < target - 1; i += 1) start += lines[i].length + 1;
    const end = start + lines[target - 1].length;
    el.focus();
    el.setSelectionRange(Math.min(end, start + Math.max(0, column - 1)), end);
    const lineHeight = parseFloat(getComputedStyle(el).lineHeight) || 20;
    el.scrollTop = Math.max(0, (target - 1) * lineHeight - el.clientHeight / 3);
    setCursor({ line: target, column: Math.min(column, lines[target - 1].length + 1) });
  };

  jumpRef.current = jump;

  const adopt = (fresh) => {
    const next = toEditor(fresh.text);
    file.mutate(fresh);
    setText(next);
    setBase(next);
    setConflict(false);
  };

  const runValidate = async () => {
    const checked = text;
    const result = await validate.run(toFile(checked, eol));
    // A check that could not run (the gateway did not answer) is shown from
    // validate.error below.
    if (!result) return;
    const issues = annotate(result.issues, checked);
    setReport({ ok: result.ok, title: `${plural(issues.length, 'problem')} found`, issues, forText: checked });
    if (!result.ok && issues[0]?.at) jump(issues[0].at.line, issues[0].at.column);
  };

  const runSave = async () => {
    const saved = text;
    const before = adminSecretIn(disk ?? base);
    const after = adminSecretIn(saved);
    const change = after !== before && typeof after === 'string' && after !== '' && !isReference(after) ? expectSecret(after) : null;
    const result = await save.run(toFile(saved, eol));
    setReview(false);
    if (!result) {
      change?.cancel();
      return;
    }
    const accepted = change ? await change.adopt() : true;
    onConfig(result);
    setBase(saved);
    setConflict(false);
    setReport(null);
    file.refresh();
    const notes = [];
    if (result.restart_required?.length > 0) notes.push(`Restart the gateway to apply ${result.restart_required.join(', ')}.`);
    if (change) notes.push(accepted ? 'This browser stays signed in with the new admin secret.' : 'SWITCHYARD_ADMIN_SECRET overrides the secret in the file, so the current secret stays in effect.');
    const description = notes.join(' ') || 'The gateway runs on the new configuration.';
    if (result.restart_required?.length > 0) toast.warning('switchyard.toml saved', { description });
    else toast.success('switchyard.toml saved', { description });
  };

  const runReload = async () => {
    if (dirty && !(await confirmDiscard('switchyard.toml'))) return;
    const result = await reload.run();
    let fresh = null;
    try {
      fresh = await api.get('/config/raw');
    } catch {
      fresh = null;
    }
    if (fresh) adopt(fresh);
    if (result) {
      onConfig(result);
      setReport(null);
      toast.success('Reloaded from disk', { description: 'The gateway read switchyard.toml again and applied it.' });
    }
  };

  const discard = () => {
    // Back to the file as it is on disk now, which also settles a conflict.
    setText(disk ?? base);
    setBase(disk ?? base);
    setConflict(false);
    setReport(null);
    save.reset();
    validate.reset();
  };

  // The diff is kept while the dialog animates out.
  if (review) reviewed.current = { before: disk ?? base, after: text };

  const secretBefore = review ? adminSecretIn(disk ?? base) : undefined;
  const secretAfter = review ? adminSecretIn(text) : undefined;
  const secretLost = review && secretAfter !== secretBefore && (typeof secretAfter !== 'string' || secretAfter === '' || isReference(secretAfter));

  return html`
    <${Form}
      id=${formId}
      class="settings-form"
      onSubmit=${() => {
        if (dirty) setReview(true);
      }}
    >
      ${conflict &&
      html`<${Notice}
        tone="caution"
        title="The file was edited elsewhere while you were editing"
        action=${html`<div class="btn-group">
          <${Button}
            size="sm"
            onClick=${() => {
              setText(disk);
              setBase(disk);
              setConflict(false);
              setReport(null);
            }}
          >
            Load the new file
          <//>
          <${Button}
            size="sm"
            onClick=${() => {
              setBase(disk);
              setConflict(false);
            }}
          >
            Keep my edits
          <//>
        </div>`}
      >
        Loading the new file discards your edits. Keeping yours means saving will overwrite what was changed elsewhere; the review before saving shows exactly which lines.
      <//>`}

      <${Panel} flush>
        <div class="settings-raw-bar">
          <div class="settings-raw-file">
            <span class="mono settings-break">${file.data?.path ?? 'switchyard.toml'}</span>
            <span class="faint">${file.data?.modified_at ? `Modified ${formatDateTime(file.data.modified_at)}` : 'Modification time unknown'}${file.error ? '. Could not refresh; showing the last version loaded.' : ''}</span>
          </div>
          <div class="btn-group">
            <${Button} size="sm" icon="check" loading=${validate.loading} onClick=${runValidate}>Validate<//>
            <${Button} size="sm" icon="refresh" loading=${reload.loading} onClick=${runReload}>Reload from disk<//>
          </div>
        </div>
        <${TomlEditor}
          value=${text}
          areaRef=${area}
          marks=${marks}
          invalid=${marks.size > 0}
          onCursor=${setCursor}
          onChange=${(next) => {
            if (save.error) save.reset();
            if (reload.error) reload.reset();
            if (validate.error) validate.reset();
            setText(next);
          }}
        />
        <div class="settings-raw-foot">
          <span class="num">Ln ${cursor.line}, Col ${cursor.column}</span>
          <span class="num">${plural(countLines(text), 'line')}</span>
          <span>Comments and formatting are kept exactly as you write them.</span>
        </div>
      <//>

      <${Report} report=${shown} stale=${shownStale} onJump=${jump} />
      <${SaveBar} dirty=${dirty} saving=${save.loading} what="switchyard.toml" onDiscard=${discard} summary="Unsaved edits to switchyard.toml" saveLabel="Review and save" returnFocus=${() => area.current} />
    <//>

    <${Modal}
      open=${review}
      onClose=${() => setReview(false)}
      size="lg"
      title="Save switchyard.toml?"
      description="The gateway validates the text, writes it to disk and applies it at once. Nothing is written if it is not valid."
      dismissable=${!save.loading}
      footer=${html`
        <${Button} disabled=${save.loading} onClick=${() => setReview(false)}>Keep editing<//>
        <${Button} variant="primary" loading=${save.loading} data-autofocus="" onClick=${runSave}>Save file<//>
      `}
    >
      <div class="stack" style="--gap:var(--space-3)">
        ${conflict && html`<${Notice} tone="caution" title="This overwrites changes made elsewhere">The file changed on disk after you started editing. The lines below are compared with the file as it is now.<//>`}
        ${secretLost &&
        html`<${Notice} tone="caution" title="The admin secret changes">This page cannot read the new secret from the text (it is an environment reference, or it is gone), so you will be asked to sign in again. With no secret at all the dashboard and the admin API switch off.<//>`}
        ${reviewed.current && html`<${Diff} before=${reviewed.current.before} after=${reviewed.current.after} />`}
      </div>
    <//>
  `;
}

export function RawTab({ onConfig, refused, onValid }) {
  const [shown, setShown] = useState(revealed);
  if (!shown) {
    return html`
      <${Panel} flush>
        <${EmptyState}
          icon="lock"
          title="This file contains secrets"
          description="switchyard.toml holds the admin secret, client keys and provider keys in full. Open it where nobody is reading along."
          action=${html`<${Button}
            variant="primary"
            icon="eye"
            onClick=${() => {
              revealed = true;
              setShown(true);
            }}
          >
            Show the file
          <//>`}
        />
      <//>
    `;
  }
  return html`<${RawEditor} onConfig=${onConfig} refused=${refused} onValid=${onValid} />`;
}

// ui/tests/check.mjs asks every module under pages/ for a default export.
export default RawTab;
