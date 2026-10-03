// Logs page: the line buffer and everything that can be decided without a
// DOM (levels, matching, the text of a downloaded line). Kept free of Preact
// so it can be exercised under Node.
//
// The buffer combines two sources that overlap:
//
//   pages of GET /logs   the newest lines that match a server-side query
//                        (level, q, target), and older ones through the
//                        `before` cursor. A page also says where it ends:
//                        `last_seq`, the newest line the gateway held when
//                        it took the page, whatever the query;
//   "log" live frames    every line the gateway writes, unfiltered.
//
// The gateway numbers its lines (`seq`, one more per line), so merging is a
// matter of sorting and de-duplicating. What needs care:
//
//   - A run of live frames is complete as long as each number follows the
//     last. A break (a frame dropped for a slow connection, a reconnect)
//     starts a new run, and the lines in between have to be fetched.
//   - A page fetched under a query is complete for that query only. The
//     display filter may be narrower than the query the buffer was loaded
//     with (then it filters in memory), but never broader: `covers` says
//     which, and the page reloads when it is not.
//   - A restarted gateway numbers its lines from 1 again. The buffer keeps
//     its own, ever-growing `seq`: the gateway's number plus `base`, which a
//     restart moves past everything held. Lines of the earlier run keep
//     their place above the new ones, and nothing that compares positions
//     (the pause point, what was cleared, the cursor) has to know.

/** Least severe first, as the gateway orders them. */
export const LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];

/** Lines kept in memory. */
export const MAX_LINES = 5000;

/** Lines the gateway itself keeps: a page of this size is everything it has. */
export const GATEWAY_LINES = 2000;

/** Position of a level in LEVELS; unknown spellings count as info, as in the gateway. */
export function levelRank(level) {
  const at = LEVELS.indexOf(String(level ?? '').toLowerCase());
  return at === -1 ? 2 : at;
}

/** A level from the URL or the config, or the fallback when it is not one. */
export function parseLevel(value, fallback = 'trace') {
  const text = String(value ?? '').trim().toLowerCase();
  return LEVELS.includes(text) ? text : fallback;
}

/** Badge tone of a level: the only place a log line wears a lamp colour. */
export function levelTone(level) {
  switch (level) {
    case 'error':
      return 'stop';
    case 'warn':
      return 'caution';
    case 'info':
      return 'info';
    default:
      return 'neutral';
  }
}

// ---------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------

/**
 * A line as the page uses it, from whatever the gateway sent. Returns null
 * for something that is not a log line (no usable `seq`).
 *
 *   seq  position in the buffer: the gateway's number plus `base`
 *   n    the gateway's own number, as shown and copied
 */
export function normalizeLine(raw, base = 0) {
  if (!raw || typeof raw !== 'object') return null;
  const n = Number(raw.seq);
  if (!Number.isFinite(n) || n <= 0) return null;
  const fields = raw.fields && typeof raw.fields === 'object' && !Array.isArray(raw.fields) ? raw.fields : {};
  return {
    seq: base + n,
    n,
    at: Number.isFinite(Number(raw.at)) ? Number(raw.at) : null,
    level: typeof raw.level === 'string' && raw.level ? raw.level.toLowerCase() : 'info',
    target: typeof raw.target === 'string' ? raw.target : '',
    message: typeof raw.message === 'string' ? raw.message : String(raw.message ?? ''),
    fields,
  };
}

/** A field value as shown in a chip and searched: strings as they are, the rest as JSON. */
export function fieldText(value) {
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

const haystacks = new WeakMap();

/**
 * Everything the search looks at, lower-cased: message, target, field names
 * and values, the same places `GET /logs?q=` searches. The parts are joined
 * by line breaks, which a single-line search box cannot contain, so a match
 * never spans two parts.
 */
function haystack(line) {
  let text = haystacks.get(line);
  if (text === undefined) {
    const parts = [line.message, line.target];
    for (const [name, value] of Object.entries(line.fields)) parts.push(name, fieldText(value));
    text = parts.join('\n').toLowerCase();
    haystacks.set(line, text);
  }
  return text;
}

/**
 * A target filter as `GET /logs?target=` reads it, ignoring case:
 *   "a::b"   exactly that target
 *   "a::"    the module `a` and everything below it
 *   "a*"     every target that starts so ("*" alone: every target)
 * Returns { exact, prefix } (either may be null), or null for no filter.
 */
function targetRule(target) {
  const text = String(target ?? '').trim().toLowerCase();
  if (!text) return null;
  if (text.endsWith('*')) return { exact: null, prefix: text.replace(/\*+$/, '') };
  if (text.endsWith('::')) return { exact: text.slice(0, -2), prefix: text };
  return { exact: text, prefix: null };
}

function targetHit(rule, target) {
  const name = String(target ?? '').toLowerCase();
  return name === rule.exact || (rule.prefix !== null && name.startsWith(rule.prefix));
}

/** True when every target the filter `inner` lets through passes `outer` too. */
function targetWithin(outer, inner) {
  const wide = targetRule(outer);
  if (wide === null || wide.prefix === '') return true;
  const narrow = targetRule(inner);
  if (narrow === null) return false;
  if (narrow.exact !== null && !targetHit(wide, narrow.exact)) return false;
  return narrow.prefix === null || (wide.prefix !== null && narrow.prefix.startsWith(wide.prefix));
}

/**
 * The display filter, prepared once per change:
 *   level   least severe level shown ("trace" shows everything)
 *   q       text searched for, ignoring case
 *   target  target shown, read the way the gateway reads it (targetRule),
 *           or "" for any
 */
export function makeFilter({ level = 'trace', q = '', target = '' } = {}) {
  const text = String(q ?? '').trim();
  const name = String(target ?? '').trim();
  return {
    level: parseLevel(level),
    rank: levelRank(parseLevel(level)),
    q: text,
    needle: text.toLowerCase(),
    target: name,
    targetRule: targetRule(name),
  };
}

export function isFiltered(filter) {
  return filter.rank > 0 || filter.needle !== '' || filter.target !== '';
}

export function matches(line, filter) {
  if (filter.rank > 0 && levelRank(line.level) < filter.rank) return false;
  if (filter.targetRule && !targetHit(filter.targetRule, line.target)) return false;
  if (filter.needle && !haystack(line).includes(filter.needle)) return false;
  return true;
}

/**
 * The query to send for a display filter. The gateway applies all three
 * parts, so a page holds nothing but lines the filter shows.
 */
export function serverQuery(filter) {
  return { level: filter.rank > 0 ? filter.level : '', q: filter.q, target: filter.target };
}

/** A query with every part present. */
function asQuery(query) {
  return { level: query?.level || '', q: query?.q || '', target: query?.target || '' };
}

/** True when two queries ask the gateway for the same lines. */
export function sameQuery(a, b) {
  const x = asQuery(a);
  const y = asQuery(b);
  return x.level === y.level && x.q === y.q && x.target === y.target;
}

/**
 * True when lines loaded under `query` are all the lines `filter` can show:
 * the filter is the same or narrower. A lower level, a search text that the
 * loaded one is not part of, or a target outside the loaded one needs a
 * reload.
 */
export function covers(query, filter) {
  if (filter.rank < levelRank(parseLevel(query.level))) return false;
  if (!targetWithin(query.target, filter.target)) return false;
  const loaded = String(query.q ?? '').trim().toLowerCase();
  return !loaded || filter.needle.includes(loaded);
}

/**
 * True when every line the query `inner` returns is also returned by
 * `outer`. Older pages may be fetched under a narrower query than the
 * buffer's, never under one that merely overlaps it.
 */
export function narrows(outer, inner) {
  if (levelRank(parseLevel(inner.level)) < levelRank(parseLevel(outer.level))) return false;
  if (!targetWithin(outer.target, inner.target)) return false;
  const wide = String(outer.q ?? '').trim().toLowerCase();
  return !wide || String(inner.q ?? '').toLowerCase().includes(wide);
}

/**
 * Split `text` around the occurrences of `needle` (lower-cased already).
 * Returns [[part, isHit], ...]; a text without hits comes back as one part.
 */
export function splitHits(text, needle) {
  const source = String(text ?? '');
  if (!needle || !source) return [[source, false]];
  const lower = source.toLowerCase();
  // Lower-casing can change the length of a few characters ("İ"); offsets
  // would then point at the wrong place, so such text is left unmarked.
  if (lower.length !== source.length) return [[source, false]];
  const parts = [];
  let from = 0;
  for (;;) {
    const at = lower.indexOf(needle, from);
    if (at === -1) break;
    if (at > from) parts.push([source.slice(from, at), false]);
    parts.push([source.slice(at, at + needle.length), true]);
    from = at + needle.length;
  }
  if (from < source.length) parts.push([source.slice(from), false]);
  return parts.length ? parts : [[source, false]];
}

/** Keep the end of a long identifier: the last segments of a target say the most. */
export function tailOf(text, max) {
  const value = String(text ?? '');
  return value.length <= max ? value : `…${value.slice(value.length - max + 1)}`;
}

// ---------------------------------------------------------------------------
// Text of a line (download, copy)
// ---------------------------------------------------------------------------

const BARE = /^[^\s"=\u0000-\u001f\u007f]+$/;

function isoTime(at) {
  if (at == null) return '-';
  const date = new Date(at);
  return Number.isNaN(date.getTime()) ? '-' : date.toISOString();
}

/**
 * One line of a log file, in the format the gateway writes its own files:
 *   2026-10-02T13:09:12.123Z  INFO target: message key=value
 * Line breaks in the message are escaped so one event stays one line.
 */
export function lineToText(line) {
  let out = `${isoTime(line.at)} ${line.level.toUpperCase().padStart(5)} ${line.target}: ${line.message.replace(/\r/g, '').replace(/\n/g, '\\n')}`;
  for (const [name, value] of Object.entries(line.fields)) {
    out += ` ${name}=${typeof value === 'string' && BARE.test(value) ? value : JSON.stringify(value)}`;
  }
  return out;
}

/** The lines as the text of a .log file. */
export function linesToText(lines) {
  return lines.length ? `${lines.map(lineToText).join('\n')}\n` : '';
}

/** A line as JSON, the way the admin API sends it. */
export function lineToJson(line) {
  return JSON.stringify({ seq: line.n ?? line.seq, at: line.at, level: line.level, target: line.target, message: line.message, fields: line.fields }, null, 2);
}

/** "switchyard-2026-10-02T13-09-12Z.log": UTC like the lines inside, safe on every file system. */
export function downloadName(now = new Date()) {
  return `switchyard-${now.toISOString().slice(0, 19).replace(/:/g, '-')}Z.log`;
}

// ---------------------------------------------------------------------------
// The buffer
// ---------------------------------------------------------------------------

/** Index of the first line with seq >= `seq` (lines are sorted by seq). */
export function lowerBound(lines, seq) {
  let low = 0;
  let high = lines.length;
  while (low < high) {
    const mid = (low + high) >>> 1;
    if (lines[mid].seq < seq) low = mid + 1;
    else high = mid;
  }
  return low;
}

/** Merge two seq-sorted arrays, dropping duplicates (the first array wins). */
function mergeSorted(a, b) {
  if (b.length === 0) return a;
  if (a.length === 0) return b;
  // The common case: everything new is newer.
  if (b[0].seq > a[a.length - 1].seq) return a.concat(b);
  const out = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i].seq === b[j].seq) {
      out.push(a[i]);
      i += 1;
      j += 1;
    } else if (a[i].seq < b[j].seq) {
      out.push(a[i]);
      i += 1;
    } else {
      out.push(b[j]);
      j += 1;
    }
  }
  while (i < a.length) out.push(a[i++]);
  while (j < b.length) out.push(b[j++]);
  return out;
}

function pageLines(page, base) {
  const list = Array.isArray(page?.lines) ? page.lines : [];
  const out = [];
  for (const raw of list) {
    const line = normalizeLine(raw, base);
    if (line) out.push(line);
  }
  out.sort((x, y) => x.seq - y.seq);
  return out;
}

/**
 * The line buffer of the page.
 *
 *   lines       every line held, oldest first, unique by seq
 *   query       { level, q, target } the history part was loaded under
 *   floor       seq the next older page ends before, or null (see `cursor`)
 *   hasMore     the gateway may have older lines for `query`
 *   lastSeq     highest seq known of: seen from any source, or named by a
 *               page as the newest line the gateway held
 *   cleared     lines up to this seq were cleared from the view
 *   trimmed     older lines were dropped to stay within `maxLines`
 *   focus       the query of what the page is showing, or null: when the
 *               buffer is full, lines outside it are the first to go
 *   base        what is added to the gateway's line numbers (see the top)
 *   restarts    [{ after, at }]: the gateway restarted after seq `after`;
 *               `at` is when the new process started, when known
 *   paused      seq the view is frozen at, or null. Lines up to it are not
 *               dropped while it is set; newer ones are held next to them.
 *   heldLost    lines that arrived during the pause and were dropped again
 *               because more arrived than `maxLines`
 *
 * Methods return `{ needSync }` where it can be true: the buffer may have a
 * hole between what was fetched and what is arriving live, and the caller
 * should fetch the newest page again and hand it to `sync`.
 */
export function createLogStore({ maxLines = MAX_LINES } = {}) {
  const store = {
    lines: [],
    query: asQuery(null),
    floor: null,
    hasMore: false,
    lastSeq: 0,
    cleared: 0,
    trimmed: false,
    focus: null,
    base: 0,
    restarts: [],
    paused: null,
    heldLost: 0,
    maxLines,
  };

  // What the buffer is known to hold:
  //   - every line matching `query` from `floor` up to `covered`;
  //   - every line at all from `run.from` to `run.last`.
  // The two join when the run starts at or before `covered + 1`; otherwise
  // there is a hole between them that only a fetch can fill.
  let covered = 0;
  // The current unbroken run of live frames: `last` is the newest frame,
  // `from` the first one from which the buffer still holds every line.
  let run = null;

  const joinRun = () => {
    if (run !== null && run.from <= covered + 1 && run.last > covered) covered = run.last;
  };
  const holeBeforeRun = () => run !== null && run.from > covered + 1;

  const dropCleared = (lines) => (store.cleared > 0 ? lines.filter((line) => line.seq > store.cleared) : lines);

  const closeFloorIfCleared = () => {
    if (store.cleared > 0 && (store.floor === null || store.floor <= store.cleared + 1)) {
      store.floor = null;
      store.hasMore = false;
    }
  };

  /** Index of the first line of the gateway's current run. */
  const currentFrom = () => lowerBound(store.lines, store.base + 1);
  /** Index of the first line held back by the pause (the length when not paused). */
  const heldFrom = () => (store.paused === null ? store.lines.length : lowerBound(store.lines, store.paused + 1));

  /**
   * Lines of earlier runs stay only while this run is held from its first
   * line. With older lines of this run still to load, they would arrive in
   * the middle of the list, below lines that are older still.
   */
  const dropEarlierRuns = () => {
    if (store.floor !== null && store.lines.length > 0 && store.lines[0].seq <= store.base) store.lines = store.lines.slice(currentFrom());
  };

  /** Where the floor of a page is: its cursor, or its first line. */
  const floorOf = (page, fetched) => {
    if (page?.has_more !== true) return null;
    const next = Number(page.next_before);
    if (page.next_before != null && Number.isFinite(next)) return store.base + next;
    return fetched.length ? fetched[0].seq : null;
  };

  /**
   * Where a page ends: the newest line the gateway held when it took the
   * page (`last_seq`), as a position in the buffer. Up to there the page
   * holds every line its query returns, and the next live frame is the line
   * after it. 0 for an empty log.
   */
  const endOf = (page) => {
    const n = Number(page?.last_seq);
    return Number.isFinite(n) && n > 0 ? store.base + n : 0;
  };

  /** The `before` parameter of the next older page, or null when there is none. */
  store.cursor = () => (store.floor === null ? null : store.floor - store.base);

  /** Lines that count against `maxLines`: all of them, or those of the paused view. */
  store.used = () => heldFrom();

  /** True while lines of an earlier gateway run are held. */
  store.hasEarlierRuns = () => store.lines.length > 0 && store.lines[0].seq <= store.base;

  /**
   * Give up every line outside `query`, which must be the buffer's query or
   * a narrower one. The buffer is then complete for `query` only. Returns
   * true when lines were dropped.
   */
  store.narrow = (query) => {
    if (!query || !narrows(store.query, query)) return false;
    const keep = makeFilter(query);
    const before = store.lines.length;
    store.lines = store.lines.filter((line) => matches(line, keep));
    store.query = asQuery(query);
    // The live run goes on, but the buffer no longer holds all of it.
    if (run !== null) run.from = run.last + 1;
    return store.lines.length < before;
  };

  /** How many lines `query` would leave in the part that counts against `maxLines`. */
  store.wouldKeep = (query) => {
    const keep = makeFilter(query);
    const end = heldFrom();
    let count = 0;
    for (let i = 0; i < end; i += 1) if (matches(store.lines[i], keep)) count += 1;
    return count;
  };

  const trim = () => {
    const excess = () => (store.paused === null ? store.lines.length : store.lines.length - heldFrom()) - store.maxLines;
    if (excess() <= 0) return;
    // In a flood, the lines being looked at are worth more than the rest.
    if (store.focus) store.narrow(store.focus);
    const over = excess();
    if (over <= 0) return;
    if (store.paused !== null) {
      // The paused view stays whole. Of what arrived since, the newest
      // `maxLines` are kept; `release` deals with the hole this leaves.
      const at = heldFrom();
      store.lines = store.lines.slice(0, at).concat(store.lines.slice(at + over));
      store.heldLost += over;
      return;
    }
    store.lines = store.lines.slice(over);
    // What was dropped of this run can be fetched again, as far as the
    // gateway has it. Lines of earlier runs are gone for good.
    if (store.lines[0].seq > store.base) {
      store.trimmed = true;
      store.floor = store.lines[0].seq;
      store.hasMore = true;
      closeFloorIfCleared();
    }
  };

  /**
   * Live frames, as they arrived since the last batch. The gateway numbers
   * a line and announces it in two steps, so two lines written at the same
   * moment now and then arrive the wrong way round: the batch is put in
   * order first, and a line that comes after its successor all the same
   * (in the next batch) is taken in without disturbing the run.
   */
  store.live = (batch) => {
    const lines = [];
    for (const raw of batch) {
      const line = normalizeLine(raw, store.base);
      if (line) lines.push(line);
    }
    lines.sort((x, y) => x.seq - y.seq);
    const fresh = lines.filter((line, i) => i === 0 || line.seq !== lines[i - 1].seq);
    let started = false;
    let added = 0;
    for (const line of fresh) {
      if (run !== null && line.seq <= run.last) {
        // A repeat (the run holds it), or a line that is late: the one just
        // before the run makes the run begin there.
        if (line.seq < run.from) added += 1;
        if (line.seq === run.from - 1) run.from = line.seq;
        continue;
      }
      added += 1;
      if (run !== null && line.seq === run.last + 1) {
        run.last = line.seq;
      } else {
        run = { from: line.seq, last: line.seq };
        started = true;
      }
      if (line.seq > store.lastSeq) store.lastSeq = line.seq;
    }
    joinRun();
    if (added > 0) {
      store.lines = mergeSorted(store.lines, dropCleared(fresh));
      trim();
    }
    return { added, needSync: started && holeBeforeRun() };
  };

  /** The live connection dropped or lost frames: the next frame starts a new run. */
  store.breakRun = () => {
    run = null;
  };

  /**
   * The gateway was restarted: its lines are numbered from 1 again. What is
   * held stays, as the lines of an earlier run; everything that arrives
   * from now on is placed after it. `at` is when the new process started.
   */
  store.restart = (at = null) => {
    const last = store.restarts[store.restarts.length - 1];
    // Restarted again before a single line of the last run was seen.
    if (last && last.after === store.lastSeq) last.at = at;
    else if (store.lastSeq > 0) store.restarts.push({ after: store.lastSeq, at });
    store.base = store.lastSeq;
    covered = store.base;
    run = null;
    store.floor = null;
    store.hasMore = false;
    store.trimmed = false;
  };

  /**
   * The newest page under a new query replaces the history of the current
   * run. Lines of the live run stay: they are complete under any query. So
   * do the lines of earlier runs, which cannot be fetched again.
   *
   * The page covers its query up to where it says it ends (`last_seq`), so
   * the live frame after that line joins it without another request, also
   * when the newest line that matches is much older. `sentAt` is `lastSeq`
   * at the time the request was sent: the response covers at least that
   * far. A page fetched for a paused view ends at the pause instead: `upTo`
   * is the seq it was asked to end before.
   */
  store.reset = (page, query, sentAt = 0, upTo = null) => {
    const fetched = dropCleared(pageLines(page, store.base));
    const earlier = store.lines.slice(0, currentFrom());
    const kept = run === null ? [] : store.lines.slice(lowerBound(store.lines, Math.max(run.from, store.base + 1)));
    store.lines = earlier.concat(mergeSorted(fetched, kept));
    store.query = asQuery(query);
    store.trimmed = false;
    const last = Math.max(fetched.length ? fetched[fetched.length - 1].seq : 0, endOf(page));
    covered = Math.max(store.base, upTo !== null ? upTo - 1 : Math.max(last, sentAt));
    // A run that ends before the page begins is still catching up with it
    // (see `sync`): until its frames are in, the buffer is whole only as
    // far as the run goes.
    if (upTo === null && run !== null && fetched.length > 0 && page?.has_more === true && run.last < fetched[0].seq - 1) covered = Math.max(store.base, run.last);
    joinRun();
    if (last > store.lastSeq) store.lastSeq = last;
    store.floor = floorOf(page, fetched);
    // The run reaches further back than the page: older lines start there.
    if (store.floor !== null && run !== null && run.from < store.floor && run.from > store.base) store.floor = run.from;
    store.hasMore = store.floor !== null;
    closeFloorIfCleared();
    dropEarlierRuns();
    trim();
    return { needSync: holeBeforeRun() };
  };

  /**
   * An older page, fetched with `before = cursor()` under `query`, which must
   * be the buffer's query or a narrower one. A narrower query becomes the
   * buffer's: below the old floor the buffer is now complete only for it.
   * Returns the lines of the page.
   */
  store.older = (page, query) => {
    const fetched = dropCleared(pageLines(page, store.base));
    const before = store.lines.length;
    store.lines = mergeSorted(fetched, store.lines);
    store.query = asQuery(query);
    store.floor = floorOf(page, fetched);
    store.hasMore = store.floor !== null;
    closeFloorIfCleared();
    return { added: store.lines.length - before, lines: fetched };
  };

  /**
   * True when `sync` can take this page without giving up what is held: the
   * page reaches back to what the buffer covers, or the gateway has nothing
   * in between.
   */
  store.joins = (page) => {
    if (page?.has_more !== true) return true;
    const fetched = dropCleared(pageLines(page, store.base));
    return fetched.length === 0 || fetched[0].seq <= covered + 1;
  };

  /**
   * The newest page under the buffer's query, fetched to catch up: after a
   * reconnect, after lost frames, or on a timer while the live connection
   * is down. Not while the view is paused: it may drop lines (see below).
   */
  store.sync = (page, sentAt = 0) => {
    const fetched = dropCleared(pageLines(page, store.base));
    // Where the page ends, whether or not its newest lines match the query.
    const end = endOf(page);
    // How far the buffer is whole once the page is in: to its end, normally.
    let whole = Math.max(sentAt, end);
    if (fetched.length) {
      const first = fetched[0].seq;
      const last = fetched[fetched.length - 1].seq;
      whole = Math.max(whole, last);
      if (page?.has_more === true && first > covered + 1) {
        // The page does not reach back to what the buffer covers and the
        // gateway has more in between: everything older would sit behind a
        // hole. Keep what is complete (the page, and the live run when it
        // starts earlier) and let "load older" fetch the rest again.
        const cut = run !== null && run.from < first ? run.from : first;
        // A run that is kept and ends before the page begins: the lines in
        // between are still on their way, as frames. Until they are in, the
        // buffer is whole only as far as the run goes, however far the page
        // reaches; should they never come, the next catch-up finds the hole.
        if (cut !== first && run.last < first - 1) whole = run.last;
        store.lines = store.lines.slice(0, currentFrom()).concat(store.lines.slice(lowerBound(store.lines, cut)));
        store.floor = cut === first ? (floorOf(page, fetched) ?? first) : cut;
        store.hasMore = true;
        store.trimmed = false;
        closeFloorIfCleared();
        dropEarlierRuns();
      } else if (store.floor !== null && first < store.floor) {
        // The page reaches below what was loaded: older lines start below it.
        store.floor = floorOf(page, fetched);
        store.hasMore = store.floor !== null;
        closeFloorIfCleared();
        dropEarlierRuns();
      }
      store.lines = mergeSorted(store.lines, fetched);
      if (last > store.lastSeq) store.lastSeq = last;
    }
    if (end > store.lastSeq) store.lastSeq = end;
    covered = Math.max(covered, whole);
    joinRun();
    trim();
    return { needSync: holeBeforeRun() };
  };

  /** Freeze the view at the newest line seen. */
  store.pause = () => {
    if (store.paused === null) {
      store.paused = store.lastSeq;
      store.heldLost = 0;
    }
  };

  /**
   * End the pause. When more arrived than the buffer holds, the paused
   * lines sit behind a hole and are given up; "load older" fetches again
   * what the gateway still has. The caller should `sync` afterwards.
   */
  store.release = () => {
    if (store.paused === null) return;
    const at = heldFrom();
    const lost = store.heldLost;
    store.paused = null;
    store.heldLost = 0;
    if (lost > 0) {
      store.lines = store.lines.slice(at);
      // Older lines start below the first line that is left. An empty
      // buffer starts over from the newest line seen.
      const floor = store.lines.length > 0 ? store.lines[0].seq : store.lastSeq + 1;
      if (floor > store.base) {
        store.floor = floor;
        store.hasMore = true;
        store.trimmed = true;
        if (covered < floor - 1) covered = floor - 1;
        closeFloorIfCleared();
      }
      dropEarlierRuns();
    }
    trim();
  };

  /** Clear the view: drop what is held and hide it from later fetches too. */
  store.clear = () => {
    store.cleared = store.lastSeq;
    store.lines = [];
    store.floor = null;
    store.hasMore = false;
    store.trimmed = false;
    store.paused = null;
    store.heldLost = 0;
  };

  /** Undo `clear` for the next reset. */
  store.unclear = () => {
    store.cleared = 0;
  };

  return store;
}
