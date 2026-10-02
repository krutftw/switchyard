// Logs page: the scrolling list of lines.
//
// Only the lines near the viewport are in the document. Rows are not all the
// same height (a wrapped message, an opened line), so heights are measured
// as rows are drawn and unmeasured rows count as one estimated row. Two
// things keep the view steady while lines come and go:
//
//   follow   the list is pinned to its end after every render, until the
//            user scrolls away from it;
//   anchor   otherwise the first visible line stays where it is, whatever
//            is added above it, dropped from the top or re-measured.

import { Component, html, useEffect, useLayoutEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import { Button, CopyButton, IconButton } from '../../components/button.js';
import { highlightJson } from '../../components/code.js';
import { Badge } from '../../components/status.js';
import { api } from '../../lib/api.js';
import { formatDate, formatTime } from '../../lib/format.js';
import { href } from '../../lib/router.js';
import { fieldText, levelTone, lineToJson, lineToText, lowerBound, splitHits, tailOf } from './model.js';

/** Rows drawn beyond each edge of the viewport. */
const OVERSCAN = 12;
/** Closer to the end than this many pixels counts as "at the end". */
const END_SLACK = 12;
/** Characters of a target shown in its column; the full name is in the title and in the details. */
const TARGET_CHARS = 30;
/** Field values longer than this get a title with the whole value. */
const CHIP_TITLE_FROM = 40;

const pad2 = (value) => String(value).padStart(2, '0');

/** "2 Oct 2026 23:50:08.060 UTC+08:00" in local time. */
export function fullTime(at) {
  if (at == null) return 'Time unknown';
  const date = new Date(at);
  if (Number.isNaN(date.getTime())) return 'Time unknown';
  const offset = -date.getTimezoneOffset();
  const abs = Math.abs(offset);
  const zone = `UTC${offset < 0 ? '−' : '+'}${pad2(Math.floor(abs / 60))}:${pad2(abs % 60)}`;
  // A reference date in 1970 keeps the year in the output.
  return `${formatDate(date, new Date(0))} ${formatTime(date, { ms: true })} ${zone}`;
}

function isoTime(at) {
  if (at == null) return undefined;
  const date = new Date(at);
  return Number.isNaN(date.getTime()) ? undefined : date.toISOString();
}

/** `text` with the search hits marked. */
function marked(text, needle) {
  if (!needle) return text;
  const parts = splitHits(text, needle);
  if (parts.length === 1 && !parts[0][1]) return text;
  return parts.map(([part, hit]) => (hit ? html`<mark class="logs-hit">${part}</mark>` : part));
}

const REQUEST_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The request a line belongs to, when it names one. */
function requestIdOf(line) {
  for (const name of ['request_id', 'request']) {
    const value = line.fields[name];
    if (typeof value === 'string' && REQUEST_ID.test(value)) return value;
  }
  return null;
}

/** Request ids known to have a record. */
const recorded = new Set();

/**
 * The way from a line to its request record. The gateway also puts a
 * request id on lines of requests it keeps no record of (rejected for a
 * wrong client key, a model list), so the record is asked for before the
 * link is offered.
 *
 * known: null while asking, true when there is a record (or it could not be
 * asked: the Requests page then says what is wrong), false when there is none.
 */
function RequestLink({ id }) {
  const [known, setKnown] = useState(() => (recorded.has(id) ? true : null));
  useEffect(() => {
    if (recorded.has(id)) {
      setKnown(true);
      return undefined;
    }
    setKnown(null);
    const controller = new AbortController();
    const options = { signal: controller.signal, timeout: 10_000 };
    (async () => {
      let found;
      try {
        // The list of recent requests answers without the captured bodies.
        // A request it no longer holds may still be in the usage files,
        // which only the record itself tells.
        const recent = await api.get('/requests', { ...options, query: { q: id, limit: 1 } });
        found = Array.isArray(recent?.items) && recent.items.some((item) => item?.id === id);
        if (!found) {
          await api.get(`/requests/${encodeURIComponent(id)}`, options);
          found = true;
        }
      } catch (error) {
        if (error.aborted || controller.signal.aborted) return;
        // Not a 404: it could not be asked. The link is offered unchecked.
        setKnown(error.status !== 404);
        return;
      }
      if (controller.signal.aborted) return;
      // A record stays; "no record" can change (a request still running), so that is asked again next time.
      if (found) {
        if (recorded.size >= 500) recorded.clear();
        recorded.add(id);
      }
      setKnown(found);
    })();
    return () => controller.abort();
  }, [id]);

  if (known === false) return html`<span class="logs-norecord">The gateway kept no record of this request.</span>`;
  return html`<${Button} size="sm" iconRight="arrow-right" href=${href(`/requests/${id}`)} loading=${known === null} disabled=${known === null}>Open request<//>`;
}

// ---------------------------------------------------------------------------
// One line
// ---------------------------------------------------------------------------

function LogDetail({ line, actions, targetOn }) {
  const names = Object.keys(line.fields);
  const iso = isoTime(line.at);
  const requestId = requestIdOf(line);
  return html`
    <div class="logs-detail">
      <dl class="logs-meta">
        <div><dt>Time</dt><dd>${fullTime(line.at)} ${iso && html`<span class="logs-meta-utc">${iso}</span>`}</dd></div>
        <div><dt>Target</dt><dd>${line.target || '—'}</dd></div>
        <div><dt>Line</dt><dd>#${line.n ?? line.seq}</dd></div>
      </dl>
      ${names.length > 0
        ? html`<pre class="logs-json" tabindex="0" aria-label="Fields of this line as JSON"><code>${highlightJson(JSON.stringify(line.fields, null, 2))}</code></pre>`
        : html`<p class="logs-nofields">This line has no structured fields.</p>`}
      <div class="logs-detail-actions">
        <${CopyButton} variant="secondary" value=${() => lineToText(line)}>Copy line<//>
        <${CopyButton} variant="secondary" value=${() => lineToJson(line)}>Copy as JSON<//>
        ${line.target &&
        html`<${Button} size="sm" icon="filter" onClick=${() => actions.target(line.target)}>
          ${targetOn === line.target ? 'Show all targets' : 'Show only this target'}
        <//>`}
        ${requestId && html`<${RequestLink} id=${requestId} />`}
      </div>
    </div>
  `;
}

/**
 * A row re-renders only when something it shows changed: the list renders
 * on every scroll frame and every batch of new lines.
 */
class LogRow extends Component {
  shouldComponentUpdate(next) {
    const now = this.props;
    return (
      next.line !== now.line ||
      next.expanded !== now.expanded ||
      next.active !== now.active ||
      next.needle !== now.needle ||
      next.targetOn !== now.targetOn ||
      next.mark !== now.mark
    );
  }

  render({ line, expanded, active, needle, targetOn, mark, actions }) {
    // Controls inside a row are reachable with Tab only in the line the
    // keyboard is on (or one that is open): the list itself is one tab stop.
    const tab = active || expanded ? 0 : -1;
    const toggle = () => actions.toggle(line.seq);
    const onLineClick = (event) => {
      if (event.target.closest('button, a')) return;
      // Dragging across a line to select its text is not a click on it.
      if (String(globalThis.getSelection?.() ?? '') !== '') return;
      toggle();
    };
    const filtered = targetOn !== '' && targetOn === line.target;
    return html`
      <div
        class="logs-row"
        role="listitem"
        data-seq=${line.seq}
        data-level=${line.level}
        data-expanded=${expanded ? '' : undefined}
        data-active=${active ? '' : undefined}
        data-mark=${mark ? '' : undefined}
      >
        ${mark &&
        html`<div class="logs-mark">
          <span>Gateway restarted${mark.at != null ? html` at <time datetime=${isoTime(mark.at)} title=${fullTime(mark.at)}>${formatTime(mark.at)}</time>` : null}</span>
        </div>`}
        <div class="logs-line" onClick=${onLineClick}>
          <time class="logs-time" datetime=${isoTime(line.at)} title=${fullTime(line.at)}>${formatTime(line.at, { ms: true })}</time>
          <span class="logs-level"><${Badge} mono tone=${levelTone(line.level)} outline=${line.level === 'trace'}>${line.level}<//></span>
          <button
            type="button"
            class="logs-target"
            tabindex=${tab}
            title=${filtered ? `${line.target}: show all targets` : `${line.target}: show only this target`}
            aria-pressed=${filtered ? 'true' : 'false'}
            onClick=${() => actions.target(line.target)}
          >
            ${marked(tailOf(line.target, TARGET_CHARS), needle)}
          </button>
          <span class="logs-text">
            <span class="logs-msg">${marked(line.message, needle)}</span>
            ${Object.entries(line.fields).map(([name, value]) => {
              const text = fieldText(value);
              return html`
                <button
                  type="button"
                  class="logs-chip"
                  key=${name}
                  tabindex=${tab}
                  aria-expanded=${expanded ? 'true' : 'false'}
                  title=${text.length > CHIP_TITLE_FROM ? `${name}=${text}` : undefined}
                  onClick=${toggle}
                >
                  <span class="logs-chip-key">${marked(name, needle)}</span><span class="logs-chip-eq">=</span><span class="logs-chip-value">${marked(text, needle)}</span>
                </button>
              `;
            })}
          </span>
        </div>
        <div class="logs-row-actions">
          <${CopyButton} value=${() => lineToText(line)} label="Copy line" tabindex=${tab} />
          <${IconButton}
            icon=${expanded ? 'chevron-up' : 'chevron-down'}
            label=${expanded ? 'Hide details' : 'Show details'}
            size="sm"
            tabindex=${tab}
            aria-expanded=${expanded ? 'true' : 'false'}
            onClick=${toggle}
          />
        </div>
        ${expanded && html`<${LogDetail} line=${line} actions=${actions} targetOn=${targetOn} />`}
      </div>
    `;
  }
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

/** Largest index i with offsets[i] <= y (offsets ascending, offsets[0] = 0). */
function indexAt(offsets, y) {
  let low = 0;
  let high = offsets.length - 1;
  while (low < high) {
    const mid = (low + high + 1) >>> 1;
    if (offsets[mid] <= y) low = mid;
    else high = mid - 1;
  }
  return low;
}

/**
 * lines      lines to show, oldest first (already filtered)
 * needle     lower-cased search text to mark, or ""
 * expanded   Set of seq whose details are open
 * activeSeq  seq of the line the keyboard is on, or null
 * targetOn   the target filter in force, or ""
 * marks      Map of seq -> { at }: the gateway restarted just before that line
 * follow     keep the end in view; `onFollow(bool)` reports when the user
 *            scrolls away from the end or back to it
 * layoutKey  changes when every row may have a new height (the wrap setting)
 * actions    { toggle(seq), target(name), activate(seq | null), copy(line) };
 *            must keep its identity between renders
 * top        content above the first line (older lines, the start of the log)
 * controls   ref the list fills with { focus() }
 */
export function LogList({ lines, needle, expanded, activeSeq, targetOn, marks, follow, onFollow, layoutKey, actions, top, controls }) {
  const scroller = useRef(null);
  const rowsEl = useRef(null);
  const topEl = useRef(null);
  const [, setTick] = useState(0);
  const rerender = () => setTick((tick) => (tick + 1) % 1_000_000);

  const state = useRef({
    heights: new Map(), // seq -> measured height, only where it differs from the estimate
    estimate: 24,
    estimateKnown: false,
    scrollTop: 0,
    viewHeight: 0,
    width: 0,
    topHeight: 0,
    anchor: null, // { seq, delta }: the line at the top of the view and how far it is scrolled past
    passes: 0,
    frame: 0,
    frameTimer: null,
    layoutKey,
    lines,
    offsets: new Float64Array(1),
    follow,
  }).current;

  if (state.layoutKey !== layoutKey) {
    state.layoutKey = layoutKey;
    state.heights.clear();
    state.estimateKnown = false;
  }

  // Where each row starts, with the heights known so far.
  const count = lines.length;
  const offsets = new Float64Array(count + 1);
  for (let i = 0; i < count; i += 1) {
    offsets[i + 1] = offsets[i] + (state.heights.get(lines[i].seq) ?? state.estimate);
  }
  const total = offsets[count];
  state.lines = lines;
  state.offsets = offsets;
  state.follow = follow;
  state.keyboardOn = activeSeq;

  // The window: what the viewport will show once this render is positioned.
  const viewHeight = state.viewHeight || 600;
  let viewTop;
  if (follow) {
    viewTop = Math.max(0, state.topHeight + total - viewHeight);
  } else if (state.anchor && count > 0) {
    const at = Math.min(count - 1, lowerBound(lines, state.anchor.seq));
    viewTop = state.topHeight + offsets[at] + state.anchor.delta;
  } else {
    viewTop = state.scrollTop;
  }
  const y = viewTop - state.topHeight;
  // What sits above the first line has scrolled out of view. Its button
  // must then not be the next tab stop: focusing it would take the log to
  // its first line.
  const topAway = state.topHeight > 0 && y >= 0 && count > 0;
  const start = Math.max(0, indexAt(offsets, Math.max(0, y)) - OVERSCAN);
  const end = Math.min(count, indexAt(offsets, Math.max(0, y + viewHeight)) + 1 + OVERSCAN);

  const updateAnchor = () => {
    const el = scroller.current;
    if (!el || state.lines.length === 0) {
      state.anchor = null;
      return;
    }
    const top = el.scrollTop - state.topHeight;
    const at = Math.min(state.lines.length - 1, indexAt(state.offsets, Math.max(0, top)));
    state.anchor = { seq: state.lines[at].seq, delta: top - state.offsets[at] };
  };

  const atEnd = (el) => el.scrollHeight - el.scrollTop - el.clientHeight < END_SLACK;

  const onScroll = () => {
    const el = scroller.current;
    if (!el) return;
    // Scroll events arrive a frame late. One that finds the position where
    // this list last put it is the echo of that, or of content that grew
    // under a resting viewport: only a position that moved is the user's.
    const moved = Math.abs(el.scrollTop - state.scrollTop) > 0.5;
    state.scrollTop = el.scrollTop;
    if (moved) {
      updateAnchor();
      const ended = atEnd(el);
      if (ended !== state.follow) onFollow(ended);
    }
    // Draw the rows for the new position once per frame. The timer is for
    // a page that gets no frames (a hidden tab that is scrolled by script).
    if (!state.frame) {
      const draw = () => {
        if (!state.frame) return;
        cancelAnimationFrame(state.frame);
        clearTimeout(state.frameTimer);
        state.frame = 0;
        rerender();
      };
      state.frame = requestAnimationFrame(draw);
      state.frameTimer = setTimeout(draw, 120);
    }
  };

  // Measure what was drawn, then put the viewport where it belongs.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    let dirty = false;

    if (el.clientWidth !== state.width) {
      // A new width re-wraps every wrapped row, and a narrow log lays its
      // lines out on two rows: nothing measured so far can be trusted.
      if (state.width !== 0) {
        state.heights.clear();
        state.estimateKnown = false;
        dirty = true;
      }
      state.width = el.clientWidth;
    }
    if (el.clientHeight !== state.viewHeight) {
      state.viewHeight = el.clientHeight;
      dirty = true;
    }
    const topHeight = topEl.current ? topEl.current.getBoundingClientRect().height : 0;
    if (Math.abs(topHeight - state.topHeight) > 0.5) {
      state.topHeight = topHeight;
      dirty = true;
    }

    const rows = rowsEl.current ? rowsEl.current.children : [];
    if (!state.estimateKnown) {
      for (const node of rows) {
        if (node.hasAttribute('data-expanded') || node.hasAttribute('data-mark')) continue;
        const height = node.getBoundingClientRect().height;
        if (height > 0) {
          if (Math.abs(height - state.estimate) > 0.5) {
            state.estimate = height;
            state.heights.clear();
            dirty = true;
          }
          state.estimateKnown = true;
        }
        break;
      }
    }
    for (const node of rows) {
      const seq = Number(node.dataset.seq);
      const height = node.getBoundingClientRect().height;
      const known = state.heights.get(seq) ?? state.estimate;
      if (Math.abs(known - height) <= 0.5) continue;
      if (Math.abs(height - state.estimate) <= 0.5) state.heights.delete(seq);
      else state.heights.set(seq, height);
      dirty = true;
    }

    // "Load older" was pressed and the lines it brought pushed it out of
    // view: the focus it had moves to the list, where the arrow keys work.
    if (topEl.current && topEl.current.inert && topEl.current.contains(document.activeElement)) el.focus({ preventScroll: true });

    // New measurements move the rows: draw again before positioning. The
    // pass count stops a layout that never settles from looping.
    if (dirty && state.passes < 6) {
      state.passes += 1;
      rerender();
      return;
    }
    state.passes = 0;

    if (state.follow) {
      // The viewport is not where this list left it and not at the end: the
      // user has just scrolled away, and the scroll event that says so is
      // still on its way. Pinning now would take the scroll back.
      if (Math.abs(el.scrollTop - state.scrollTop) > 0.5 && !atEnd(el)) return;
      const max = el.scrollHeight - el.clientHeight;
      if (Math.abs(el.scrollTop - max) > 0.5) el.scrollTop = max;
    } else if (state.anchor && state.lines.length > 0) {
      const at = Math.min(state.lines.length - 1, lowerBound(state.lines, state.anchor.seq));
      const want = state.topHeight + state.offsets[at] + state.anchor.delta;
      if (Math.abs(el.scrollTop - want) > 0.5) el.scrollTop = want;
    }
    state.scrollTop = el.scrollTop;

    // A list that fits its viewport cannot be scrolled away from its end:
    // without a line being read, it follows.
    if (!state.follow && state.keyboardOn == null && el.scrollHeight <= el.clientHeight + 0.5) onFollow(true);
  });

  // The viewport itself changes size with the window and the toolbar.
  useEffect(() => {
    const el = scroller.current;
    if (!el || typeof ResizeObserver === 'undefined') return undefined;
    const observer = new ResizeObserver(() => rerender());
    observer.observe(el);
    return () => {
      observer.disconnect();
      if (state.frame) cancelAnimationFrame(state.frame);
      clearTimeout(state.frameTimer);
      state.frame = 0;
    };
  }, []);

  /** Scroll so that row `index` is in view. Returns true when the list moved. */
  const reveal = (index) => {
    const el = scroller.current;
    if (!el) return false;
    const rowTop = state.topHeight + state.offsets[index];
    const rowBottom = state.topHeight + state.offsets[index + 1];
    const from = el.scrollTop;
    if (rowTop < from) el.scrollTop = rowTop;
    else if (rowBottom > from + el.clientHeight) el.scrollTop = rowBottom - el.clientHeight;
    if (Math.abs(el.scrollTop - from) <= 0.5) return false;
    // This is where the view now belongs: without it the next render would
    // put the list back where the old anchor says (the scroll event that
    // would move the anchor comes a frame later).
    state.scrollTop = el.scrollTop;
    updateAnchor();
    return true;
  };

  const move = (delta) => {
    const list = state.lines;
    const el = scroller.current;
    if (list.length === 0 || !el) return;
    let index = activeSeq == null ? -1 : lowerBound(list, activeSeq);
    if (index === -1 || index >= list.length || list[index].seq !== activeSeq) {
      // No line has the keyboard yet: start from what is on screen.
      const top = el.scrollTop - state.topHeight;
      index = delta > 0 ? indexAt(state.offsets, Math.max(0, top)) : indexAt(state.offsets, Math.max(0, top + el.clientHeight - 1));
      index = Math.min(list.length - 1, index);
    } else {
      index = Math.max(0, Math.min(list.length - 1, index + delta));
    }
    // Reading a line and following the end do not go together.
    if (state.follow && index < list.length - 1) {
      updateAnchor();
      onFollow(false);
    }
    actions.activate(list[index].seq);
    // Back on the last line: follow the end again.
    if (index === list.length - 1) {
      if (!state.follow) onFollow(true);
    } else if (reveal(index)) {
      rerender();
    }
  };

  const onKeyDown = (event) => {
    // Keys pressed on a control inside a row belong to that control.
    if (event.target !== scroller.current) return;
    if (event.ctrlKey || event.metaKey || event.altKey) return;
    const active = activeSeq == null ? null : state.lines[lowerBound(state.lines, activeSeq)];
    const line = active && active.seq === activeSeq ? active : null;
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault();
        move(1);
        break;
      case 'ArrowUp':
        event.preventDefault();
        move(-1);
        break;
      case 'Enter':
      case ' ':
        if (!line) return;
        event.preventDefault();
        actions.toggle(line.seq);
        break;
      case 'c':
      case 'C':
        if (!line) return;
        event.preventDefault();
        actions.copy(line);
        break;
      case 'Escape':
        if (activeSeq == null) return;
        event.preventDefault();
        actions.activate(null);
        // Letting go of a line at the end of the log resumes following.
        if (!state.follow && scroller.current && atEnd(scroller.current)) onFollow(true);
        break;
      default:
    }
  };

  if (controls) {
    controls.current = {
      focus: () => scroller.current?.focus({ preventScroll: true }),
    };
  }

  return html`
    <div
      ref=${scroller}
      class="logs-scroll"
      tabindex="0"
      role="region"
      aria-label="Log lines"
      aria-describedby="logs-keys"
      onScroll=${onScroll}
      onKeyDown=${onKeyDown}
    >
      <div ref=${topEl} class="logs-top" inert=${topAway}>${top}</div>
      <div style=${`height:${offsets[start]}px`} aria-hidden="true"></div>
      <div ref=${rowsEl} class="logs-rows" role="list">
        ${lines.slice(start, end).map(
          (line) => html`
            <${LogRow}
              key=${line.seq}
              line=${line}
              expanded=${expanded.has(line.seq)}
              active=${line.seq === activeSeq}
              needle=${needle}
              targetOn=${targetOn}
              mark=${marks ? marks.get(line.seq) : undefined}
              actions=${actions}
            />
          `,
        )}
      </div>
      <div style=${`height:${total - offsets[end]}px`} aria-hidden="true"></div>
    </div>
  `;
}

// Modules under js/pages/ are checked for a default export (ui/tests/check.mjs).
export default LogList;
