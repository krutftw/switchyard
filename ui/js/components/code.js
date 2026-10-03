// CodeBlock: bodies, payloads, config snippets.
//
//   html`<${CodeBlock} title="Upstream request" value=${record.upstream_request} />`
//   html`<${CodeBlock} value=${'curl -s http://127.0.0.1:8317/v1/models'} language="text" />`
//
// JSON is re-indented and lightly coloured (keys, strings, numbers,
// literals). Re-indenting never changes a token: what is shown, and what the
// copy button copies, is what was sent, only with different white space.
// The text is rendered as text nodes, never as HTML, so captured bodies
// cannot inject markup.

import { html, useMemo } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';
import { useLocalStorage } from '../lib/hooks.js';
import { CopyButton, IconButton } from './button.js';

// Above this size colouring is skipped: tens of thousands of spans cost more
// than they are worth.
const HIGHLIGHT_LIMIT = 200_000;
const FORMAT_DEPTH_LIMIT = 64;
const FORMAT_OUTPUT_LIMIT = 1_000_000;

// A JSON string, written as an unrolled loop: the obvious (?:\\.|[^"\\])*
// overflows the regex engine's stack on a string of a few megabytes, and a
// request body with an inline base64 image is exactly that.
const STRING = '"[^"\\\\]*(?:\\\\.[^"\\\\]*)*"';
const NUMBER = '-?\\d+(?:\\.\\d+)?(?:[eE][+-]?\\d+)?';

const JSON_TOKEN = new RegExp(`(${STRING})(\\s*:)?|\\b(true|false|null)\\b|(${NUMBER})|([{}[\\],:])`, 'g');

/** Split JSON text into coloured spans and plain strings. Exported for tests. */
export function highlightJson(text) {
  // Only lex complete, bounded JSON. Partial tool arguments and truncated
  // captures are plain text; a failed string must never be retried at each
  // later quote by the regular expression.
  if (text.length > HIGHLIGHT_LIMIT) return [text];
  try {
    JSON.parse(text);
  } catch {
    return [text];
  }
  const out = [];
  let last = 0;
  JSON_TOKEN.lastIndex = 0;
  let match;
  while ((match = JSON_TOKEN.exec(text)) !== null) {
    if (match.index > last) out.push(text.slice(last, match.index));
    if (match[1] !== undefined) {
      if (match[2] !== undefined) {
        out.push(html`<span class="tok-key">${match[1]}</span>`);
        out.push(html`<span class="tok-punct">${match[2]}</span>`);
      } else {
        out.push(html`<span class="tok-string">${match[1]}</span>`);
      }
    } else if (match[3] !== undefined) {
      out.push(html`<span class="tok-atom">${match[3]}</span>`);
    } else if (match[4] !== undefined) {
      out.push(html`<span class="tok-number">${match[4]}</span>`);
    } else {
      out.push(html`<span class="tok-punct">${match[5]}</span>`);
    }
    last = JSON_TOKEN.lastIndex;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

const JSON_LEX = new RegExp(`${STRING}|${NUMBER}|true|false|null|[{}[\\],:]`, 'g');

/**
 * Re-indent JSON text (two spaces, like JSON.stringify) without changing a
 * single token. Returns null when `text` is not valid JSON or exceeds the
 * formatting size/depth budget; the caller then shows the original text.
 *
 * It does not go through JSON.parse + JSON.stringify, because that rewrites
 * what was on the wire: integers above 2^53 are rounded (a 64-bit `seed`),
 * `1.0` becomes `1`, `1e3` becomes `1000`, duplicate keys collapse and
 * escapes are normalised. For a captured request or response body the exact
 * text is the point. Exported for tests.
 */
export function formatJson(text) {
  if (text.length > HIGHLIGHT_LIMIT) return null;
  try {
    // Only to learn whether it is JSON; the parsed value is not used.
    JSON.parse(text);
  } catch {
    return null;
  }
  let out = '';
  let depth = 0;
  let prev = '';
  const newline = () => `\n${'  '.repeat(depth)}`;
  JSON_LEX.lastIndex = 0;
  let match;
  while ((match = JSON_LEX.exec(text)) !== null) {
    const token = match[0];
    if (token === '}' || token === ']') {
      depth -= 1;
      // An empty container stays on one line: {} and [].
      out += prev === '{' || prev === '[' ? token : newline() + token;
    } else if (token === ',') {
      out += token;
    } else if (token === ':') {
      out += ': ';
    } else {
      if (prev === '{' || prev === '[' || prev === ',') out += newline();
      out += token;
      if (token === '{' || token === '[') depth += 1;
      if (depth > FORMAT_DEPTH_LIMIT) return null;
    }
    if (out.length > FORMAT_OUTPUT_LIMIT) return null;
    prev = token;
  }
  return out;
}

/** Normalise any value to display text and say whether it is JSON. */
function toText(value, language) {
  if (value == null) return { text: '', json: false };
  if (typeof value !== 'string') {
    try {
      return { text: JSON.stringify(value, null, 2), json: true };
    } catch {
      return { text: String(value), json: false };
    }
  }
  if (language === 'text') return { text: value, json: false };
  const trimmed = value.trim();
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    const pretty = formatJson(trimmed);
    if (pretty !== null) return { text: pretty, json: true };
    // Not valid JSON (truncated capture, SSE text): shown as it is.
  }
  return { text: value, json: language === 'json' };
}

/**
 * value      a string, or any JSON-serialisable value. For a captured body
 *            pass the string as it was captured: it is re-indented token by
 *            token, so numbers, key order, duplicate keys and escapes are
 *            shown and copied exactly as they were sent. An object has
 *            already been through JSON.parse and can only be shown as
 *            JavaScript sees it (large integers rounded, 1.0 as 1).
 * language   "auto" (default): objects and JSON-looking strings are
 *            pretty-printed and coloured; "json": always colour; "text": never
 * title      label in the header bar (text or markup); without it the bar
 *            holds only the tools, at its right end. The tools never sit
 *            over the code.
 * label      accessible name of the scrolling block. Default: the title
 *            when it is a string, else "Code". Give one when the title is
 *            markup.
 * wrap       initial wrap state when the user has no stored preference
 *            (default false: long lines scroll sideways)
 * copy       show the copy button (default true)
 * maxHeight  CSS max-height before the block scrolls (default 420px)
 * note       quiet footer line ("Truncated at 64 KB")
 * actions    extra controls in the tools row
 */
export function CodeBlock({ value, language = 'auto', title, label, wrap = false, copy = true, maxHeight = '420px', note, actions, class: className }) {
  const [wrapped, setWrapped] = useLocalStorage('code.wrap', wrap);
  const { text, json } = useMemo(() => toText(value, language), [value, language]);
  const body = useMemo(() => (json && text.length <= HIGHLIGHT_LIMIT ? highlightJson(text) : text), [text, json]);

  const tools = html`
    <div class="code-tools">
      ${actions}
      <${IconButton}
        icon="wrap"
        label=${wrapped ? 'Do not wrap lines' : 'Wrap long lines'}
        size="sm"
        aria-pressed=${wrapped ? 'true' : 'false'}
        onClick=${() => setWrapped(!wrapped)}
      />
      ${copy && html`<${CopyButton} value=${text} label="Copy" />`}
    </div>
  `;

  // The tools always sit in a bar of their own, above the code: tools
  // floating over the block would cover the end of a long first line, and a
  // line that scrolls sideways passes under them whatever room is reserved.
  return html`
    <div class=${cx('code', className)} data-wrap=${wrapped ? '' : undefined}>
      <div class="code-bar" data-untitled=${title ? undefined : ''}>${title && html`<span class="code-title">${title}</span>`}${tools}</div>
      <pre class="code-pre" tabindex="0" style=${maxHeight ? `max-height:${maxHeight}` : undefined} aria-label=${label || (typeof title === 'string' && title) || 'Code'}><code>${body}</code></pre>
      ${note && html`<div class="code-note">${note}</div>`}
    </div>
  `;
}
