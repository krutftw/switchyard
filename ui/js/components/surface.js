// Surfaces and the content blocks that sit on them:
// Page, Panel (alias Card), Notice, Stat, StatGroup, KeyValue, Skeleton,
// EmptyState, ErrorState, Pagination, LoadMore, Timeline.

import { html, useEffect, useRef } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';
import { DASH, formatDelta, formatNumber, sentence } from '../lib/format.js';
import { useFocusHandOff, useUid } from '../lib/hooks.js';
import { Button, CopyButton, IconButton } from './button.js';
import { Icon } from './icons.js';
import { StatusLamp, toneWord } from './status.js';

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/**
 * The frame of every routed page: sets the document title and renders the
 * heading row.
 *
 *   html`<${Page} title="Providers" description="Upstreams the gateway can route to."
 *                actions=${html`<${Button} variant="primary" icon="plus">Add provider<//>`}>
 *     ...
 *   <//>`
 *
 * title        page heading (h1) and browser tab title
 * description  one sentence under the heading; optional
 * actions      buttons aligned to the right of the heading
 */
export function Page({ title, description, actions, class: className, children }) {
  useEffect(() => {
    if (title) document.title = `${title} · Switchyard`;
  }, [title]);
  return html`
    <div class=${cx('page', className)}>
      <header class="page-head">
        <div class="page-head-text">
          <h1>${title}</h1>
          ${description && html`<p class="page-desc">${description}</p>`}
        </div>
        ${actions && html`<div class="page-actions">${actions}</div>`}
      </header>
      ${children}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Panel / Card
// ---------------------------------------------------------------------------

/**
 * A bordered surface with an optional header and footer.
 *
 * title        panel heading (h2)
 * description  quieter line under the title
 * actions      controls on the right of the header
 * footer       content of the footer row
 * flush        no body padding: for tables, code and lists that run edge to edge
 *
 * The section is named by its title for assistive technology (the heading
 * has an id and the section is aria-labelledby it), so a titled panel is a
 * region a screen reader can list and jump to. Pass aria-label (or your own
 * aria-labelledby) to name it differently, or to name a panel with no title.
 *
 * Do not nest panels. Inside a panel, separate things with a hairline
 * (<hr>) or with space.
 */
export function Panel({ title, description, actions, footer, flush = false, class: className, children, ...rest }) {
  const hasHead = title != null || actions != null;
  const titleId = useUid('panel-title');
  const named = rest['aria-label'] != null || rest['aria-labelledby'] != null;
  return html`
    <section class=${cx('panel', className)} data-flush=${flush ? '' : undefined} aria-labelledby=${title != null && !named ? titleId : undefined} ...${rest}>
      ${hasHead &&
      html`
        <header class="panel-head">
          <div class="panel-head-text">
            ${title != null && html`<h2 class="panel-title" id=${titleId}>${title}</h2>`}
            ${description != null && html`<p class="panel-desc">${description}</p>`}
          </div>
          ${actions != null && html`<div class="panel-actions">${actions}</div>`}
        </header>
      `}
      <div class="panel-body">${children}</div>
      ${footer != null && html`<footer class="panel-foot">${footer}</footer>`}
    </section>
  `;
}

/** Card is Panel under the name people look for. */
export const Card = Panel;

// ---------------------------------------------------------------------------
// Notice
// ---------------------------------------------------------------------------

const NOTICE_ICON = { stop: 'alert-circle', caution: 'alert', clear: 'check-circle', info: 'info', neutral: 'info' };

/**
 * An inline message that stays on the page (unlike a toast).
 *
 * tone    "neutral" | "info" | "clear" | "caution" | "stop"
 * title   short statement of what happened
 * action  a button or link on the right
 * Errors use role="alert" so they are announced when they appear.
 *
 * A notice the page takes away while its own action has the focus (a "Try
 * again" that worked) hands the focus on to what follows it instead of
 * dropping it on <body> (useFocusHandOff in lib/hooks.js).
 */
export function Notice({ tone = 'neutral', title, icon, action, class: className, children }) {
  const root = useRef(null);
  useFocusHandOff(root);
  return html`
    <div ref=${root} class=${cx('notice', className)} data-tone=${tone === 'neutral' ? undefined : tone} role=${tone === 'stop' ? 'alert' : 'status'}>
      <${Icon} name=${icon || NOTICE_ICON[tone] || 'info'} />
      <div class="notice-body">
        ${title && html`<div class="notice-title">${title}</div>`}
        ${children != null && html`<div class="notice-text">${children}</div>`}
      </div>
      ${action}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Stat
// ---------------------------------------------------------------------------

/**
 * One headline number.
 *
 *   html`<${Stat} label="Requests, last hour" value=${formatCompact(n)}
 *               delta=${0.124} deltaLabel="vs previous hour"
 *               trend=${html`<${Sparkline} data=${points} />`} />`
 *
 * label       sentence case, no trailing colon
 * value       already formatted (use lib/format.js); a dash is shown for null
 * unit        small text after the value ("ms", "%")
 * delta       signed ratio (0.124 = +12.4%); pass a string to show it as is
 * goodWhen    "up" (default) | "down" | "none": which direction is good news
 *             and so which lamp colour the delta wears
 * deltaLabel  what the delta compares against ("vs yesterday")
 * hint        quiet text in the footer when there is no delta
 * trend       slot on the right, normally a Sparkline
 * lamp        a lamp tone shown before the label, for stats that are states
 * lampLabel   what the lamp says, in words ("Degraded", "Above 5%"): its
 *             accessible name and its tooltip. Without it the lamp is named
 *             after its tone in a plain word (toneWord: "Warning",
 *             "Critical"), so the judgement is never carried by colour
 *             alone. Pass false only when the value or the hint next to it
 *             already says the same thing in words: the lamp is then
 *             decoration, hidden from assistive technology.
 * loading     skeleton in place of the value
 */
export function Stat({ label, value, unit, delta, goodWhen = 'up', deltaLabel, hint, trend, lamp, lampLabel, loading = false, class: className }) {
  let deltaText = null;
  let deltaTone = 'flat';
  let deltaIcon = null;
  if (typeof delta === 'number' && Number.isFinite(delta)) {
    deltaText = formatDelta(delta);
    if (delta !== 0) {
      deltaIcon = delta > 0 ? 'arrow-up' : 'arrow-down';
      if (goodWhen !== 'none') deltaTone = (delta > 0) === (goodWhen === 'up') ? 'good' : 'bad';
    }
  } else if (typeof delta === 'string') {
    deltaText = delta;
  }
  return html`
    <div class=${cx('stat', className)}>
      <div class="stat-label">
        ${lamp && (lampLabel === false ? html`<span class="lamp" data-tone=${lamp} aria-hidden="true"></span>` : html`<${StatusLamp} tone=${lamp} title=${lampLabel || toneWord(lamp)} />`)}${label}
      </div>
      <div class="stat-main">
        ${loading
          ? html`<${Skeleton} width="96px" height="30px" />`
          : html`<div class="stat-value">${value ?? DASH}${unit && html`<span class="stat-unit">${unit}</span>`}</div>`}
        ${trend && !loading && html`<div class="stat-trend">${trend}</div>`}
      </div>
      <div class="stat-foot">
        ${deltaText &&
        html`<span class="stat-delta" data-tone=${deltaTone}>
          ${deltaIcon && html`<${Icon} name=${deltaIcon} size=${12} />`}${deltaText}
        </span>`}
        ${deltaText && deltaLabel && html`<span>${deltaLabel}</span>`}
        ${!deltaText && hint && html`<span>${hint}</span>`}
      </div>
    </div>
  `;
}

/**
 * Stats belong together on one instrument panel, divided by hairlines.
 * html`<${StatGroup} label="Traffic"><${Stat} ... /><${Stat} ... /><//>`
 *
 * label   accessible name of the group
 * Other props (data-*, aria-*, id) go to the group's element.
 */
export function StatGroup({ class: className, children, label, ...rest }) {
  return html`<div class=${cx('stat-group', className)} role="group" aria-label=${label} ...${rest}>${children}</div>`;
}

// ---------------------------------------------------------------------------
// KeyValue
// ---------------------------------------------------------------------------

/**
 * A definition list for record details.
 *
 *   html`<${KeyValue} items=${[
 *     { label: 'Request id', value: r.id, mono: true, copy: true },
 *     { label: 'Provider', value: r.provider },
 *     { label: 'Status', value: html`<${Badge} tone="clear">200<//>` },
 *   ]} />`
 *
 * Each item: { label, value, mono?, copy? (true, or the string to copy), hidden? }.
 * null and "" values render as a dash; `hidden: true` drops the row.
 */
export function KeyValue({ items, class: className }) {
  return html`
    <dl class=${cx('kv', className)}>
      ${items
        .filter((item) => item && !item.hidden)
        .map((item) => {
          const empty = item.value == null || item.value === '';
          const copyValue = item.copy === true ? (typeof item.value === 'string' || typeof item.value === 'number' ? String(item.value) : null) : item.copy;
          return html`
            <div class="kv-row" key=${item.label}>
              <dt>${item.label}</dt>
              <dd>
                <span class=${item.mono ? 'mono' : undefined}>${empty ? DASH : item.value}</span>
                ${copyValue && !empty && html`<${CopyButton} value=${copyValue} label=${`Copy ${String(item.label).toLowerCase()}`} />`}
              </dd>
            </div>
          `;
        })}
    </dl>
  `;
}

// ---------------------------------------------------------------------------
// Skeleton
// ---------------------------------------------------------------------------

/**
 * A placeholder with the shape of the content that is loading.
 *
 * width, height  CSS sizes for a single block
 * lines          render this many text lines instead (the last one shorter)
 * Use skeletons for a first load. While refetching, keep the old content.
 */
export function Skeleton({ width, height, lines, class: className }) {
  if (lines) {
    return html`
      <span class=${cx('skel-lines', className)} aria-hidden="true">
        ${Array.from({ length: lines }, (_, i) => html`<span class="skel" style=${`width:${i === lines - 1 && lines > 1 ? 62 : 100}%`}></span>`)}
      </span>
    `;
  }
  const style = `${width ? `width:${width};` : ''}${height ? `height:${height};` : ''}`;
  return html`<span class=${cx('skel', className)} style=${style} aria-hidden="true"></span>`;
}

// ---------------------------------------------------------------------------
// EmptyState, ErrorState
// ---------------------------------------------------------------------------

/**
 * Shown when there is nothing to list yet. Say what will appear here and how
 * to make it appear; offer the action when there is one.
 *
 *   html`<${EmptyState} icon="key" title="No client keys yet"
 *        description="Create a key and give it to an application that should use this gateway."
 *        action=${html`<${Button} variant="primary" icon="plus">Create key<//>`} />`
 *
 * icon defaults to "buffer": the end of the line.
 */
export function EmptyState({ icon = 'buffer', title, description, action, compact = false, class: className, children }) {
  return html`
    <div class=${cx('empty', className)} data-compact=${compact ? '' : undefined}>
      <div class="empty-icon"><${Icon} name=${icon} size=${20} /></div>
      ${title && html`<h3 class="empty-title">${title}</h3>`}
      ${description && html`<p class="empty-desc">${description}</p>`}
      ${children}
      ${action && html`<div class="empty-action">${action}</div>`}
    </div>
  `;
}

/**
 * Shown when loading failed. Pass the ApiError from useResource; the message
 * the gateway sent is shown, with a Retry button when `onRetry` is given.
 *
 *   if (providers.error && !providers.data)
 *     return html`<${ErrorState} error=${providers.error} onRetry=${providers.refresh} />`;
 *
 * title defaults to "Could not load this". Name the thing when you can:
 * title="Could not load providers".
 *
 * The message (`description`, else the gateway's) is printed as a sentence
 * (lib/format.js, sentence). Keep the error state on screen while the retry
 * runs and pass `retrying`: the button shows a spinner and keeps the focus.
 * When the retry works and the page swaps the error state for the content,
 * the focus goes to the first control of what took its place (else what
 * follows), not to <body> (useFocusHandOff in lib/hooks.js).
 */
export function ErrorState({ error, title = 'Could not load this', description, onRetry, retrying = false, compact = false, class: className }) {
  // Markup passed as the description is shown as it is.
  const said = description ?? error?.message;
  const message = said != null && typeof said === 'object' ? said : sentence(said) || 'The gateway did not give a reason.';
  const status = error?.status;
  const root = useRef(null);
  useFocusHandOff(root);
  return html`
    <div ref=${root} class=${cx('empty', className)} data-tone="stop" data-compact=${compact ? '' : undefined} role="alert">
      <div class="empty-icon"><${Icon} name="alert" size=${20} /></div>
      <h3 class="empty-title">${title}</h3>
      <p class="empty-desc">${message}</p>
      ${status > 0 && html`<p class="empty-detail">HTTP ${status}</p>`}
      ${onRetry &&
      html`<div class="empty-action">
        <${Button} icon="refresh" loading=${retrying} onClick=${() => onRetry()}>Try again<//>
      </div>`}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Pagination, LoadMore
// ---------------------------------------------------------------------------

/**
 * Page-number paging for lists whose total is known.
 *
 * page       1-based current page
 * pageSize   rows per page
 * total      total rows
 * onPage     (page) => void
 * noun       what is being counted, for the summary ("keys")
 *
 * Previous on the first page and Next on the last are aria-disabled, not
 * disabled: they look off and ignore clicks but keep the focus, so pressing
 * Next onto the last page leaves the keyboard where it was.
 */
export function Pagination({ page, pageSize, total, onPage, noun = 'rows', class: className }) {
  const pageCount = Math.max(1, Math.ceil((total || 0) / pageSize));
  const current = Math.min(Math.max(1, page), pageCount);
  const from = total === 0 ? 0 : (current - 1) * pageSize + 1;
  const to = Math.min(total, current * pageSize);
  const first = current <= 1;
  const last = current >= pageCount;
  return html`
    <nav class=${cx('pager', className)} aria-label="Pagination">
      <span><span class="num">${formatNumber(from)}–${formatNumber(to)}</span> of <span class="num">${formatNumber(total)}</span> ${noun}</span>
      <div class="pager-nav">
        <${IconButton} icon="chevron-left" label="Previous page" size="sm" aria-disabled=${first ? 'true' : undefined} onClick=${() => !first && onPage(current - 1)} />
        <span class="pager-page" aria-current="page">${current} / ${pageCount}</span>
        <${IconButton} icon="chevron-right" label="Next page" size="sm" aria-disabled=${last ? 'true' : undefined} onClick=${() => !last && onPage(current + 1)} />
      </div>
    </nav>
  `;
}

/**
 * Cursor paging for streams (requests, logs): a button that fetches the next
 * batch of older rows.
 *
 * hasMore  false hides the button and shows the end-of-list line
 * loading  a fetch is in flight
 * onLoad   () => void
 * shown    rows on screen, for the summary line; optional
 * noun     what the rows are: a pair [one, many] (['request', 'requests']),
 *          or a plural word ending in "s" ("requests"; default "rows")
 *          whose singular is that word without the "s". The end-of-list
 *          line reads "All 12 requests shown", or "The only request is
 *          shown" for one.
 *
 * When the last batch has come in while the button had the focus, the
 * button gives way to the end-of-list line and the focus goes to that line,
 * not to <body>; unless the page has placed it meanwhile (on the first of
 * the rows that arrived, say), which wins.
 */
export function LoadMore({ hasMore, loading = false, onLoad, shown, noun = 'rows', class: className }) {
  const many = Array.isArray(noun) ? (noun[1] ?? `${noun[0]}s`) : String(noun);
  const one = Array.isArray(noun) ? noun[0] : many.endsWith('s') && !many.endsWith('ss') ? many.slice(0, -1) : many;
  const end = shown == null ? `No more ${many}` : shown === 1 ? `The only ${one} is shown` : `All ${formatNumber(shown)} ${many} shown`;
  const endLine = useRef(null);
  return html`
    <div class=${cx('loadmore', className)}>
      ${hasMore
        ? html`<${LoadMoreButton} loading=${loading} onLoad=${onLoad} endLine=${endLine}>Load older ${many}<//>`
        : html`<span class="loadmore-end" tabindex="-1" ref=${endLine}>${end}</span>`}
    </div>
  `;
}

/** The button of LoadMore: its own component, so it can hand the focus on when it goes. */
function LoadMoreButton({ loading, onLoad, endLine, children }) {
  const root = useRef(null);
  useFocusHandOff(root, { to: () => endLine.current });
  return html`<span class="loadmore-action" ref=${root}><${Button} size="sm" loading=${loading} onClick=${() => onLoad()}>${children}<//></span>`;
}

// ---------------------------------------------------------------------------
// Timeline
// ---------------------------------------------------------------------------

/**
 * A vertical sequence of events with a lamp at each: built for the attempts
 * of a request (which credential was tried, what happened, how long it took).
 *
 *   html`<${Timeline} items=${record.attempts.map((a, i) => ({
 *     tone: a.ok ? 'clear' : 'stop',
 *     title: `${a.provider} · ${a.credential_label}`,
 *     badges: html`<${Badge} mono tone=${toneForStatus(a.status)}>${a.status}<//>`,
 *     time: formatDuration(a.duration_ms),
 *     description: a.error,
 *   }))} />`
 *
 * Each item: { tone, toneLabel?, title, badges?, time?, description?, key? }.
 * `toneLabel` is what the lamp says in words ("Failed", "Retried"): its
 * accessible name and tooltip. Default: toneWord(tone).
 */
export function Timeline({ items, class: className }) {
  return html`
    <ol class=${cx('timeline', className)}>
      ${items.map(
        (item, i) => html`
          <li class="timeline-item" key=${item.key ?? i}>
            <span class="timeline-node"><${StatusLamp} tone=${item.tone || 'off'} title=${item.toneLabel || toneWord(item.tone)} /></span>
            <div class="timeline-main">
              <div class="timeline-top">
                <div class="timeline-title">${item.title}${item.badges}</div>
                ${item.time != null && html`<span class="timeline-time">${item.time}</span>`}
              </div>
              ${item.description && html`<div class="timeline-desc">${item.description}</div>`}
            </div>
          </li>
        `,
      )}
    </ol>
  `;
}
