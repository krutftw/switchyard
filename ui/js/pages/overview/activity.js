// Overview: the live activity feed. The most recent requests, newest first:
// a row appears when a request starts (request.started), shows that it is in
// flight, and settles in place when it ends (request.finished). Each row
// links to the request on the Requests page.
//
// The gateway lists finished requests only (GET /requests) and announces a
// start only to a page that is listening. So a request that began before
// this page was opened cannot be listed until it ends; the feed says how
// many such requests there are instead of leaving them out silently. Rows
// that were in flight when the operator went to another page are carried
// over and shown again on return.

import { html, useEffect, useRef, useState } from '../../../vendor/preact-htm.js';
import { Badge, Button, EmptyState, ErrorState, IconButton, Panel, Skeleton, StatusLamp, toneForStatus } from '../../components/index.js';
import { formatDuration, formatRelativeTime, formatTokens, plural } from '../../lib/format.js';
import { useLive } from '../../lib/live.js';
import { href } from '../../lib/router.js';
import { useStore } from '../../lib/store.js';
import { gauges, useServerNow } from './data.js';
import { applyFeed, isPending, tokensOf } from './model.js';
import { described, focusSoon, focusTarget, isStale } from './stale.js';

export const FEED_CAP = 15;
// Frames are applied in batches: a busy gateway sends hundreds a second, and
// a list that changes faster than this cannot be read anyway.
const FLUSH_MS = 300;
// While paused, frames wait here. Only the newest matter for a list of 15.
const QUEUE_CAP = 600;
export const FEED_POLL_MS = 5_000;
// A row is taken for abandoned (its end was missed) only when it is older than this.
const SETTLE_MS = 3_000;

const ERROR_KIND = {
  invalid_request: 'invalid request',
  not_found: 'model not found',
  rate_limit: 'rate limited',
  upstream: 'upstream error',
  unavailable: 'no provider available',
  timeout: 'timed out',
  auth: 'not authorised',
};

// Rows in flight when the page was left, for the next visit.
let carried = [];

function Row({ record, now, fresh }) {
  const pending = isPending(record);
  const tokens = tokensOf(record);
  const key = record.client?.key_name ?? 'anonymous';
  const model = record.requested_model ?? record.client_model ?? 'no model';
  const lampTitle = pending ? 'In flight' : record.ok ? 'Succeeded' : 'Failed';
  const tone = pending ? 'info' : record.ok ? 'clear' : toneForStatus(record.status);
  const kind = record.error?.kind;
  const first = pending ? (record.endpoint ?? record.client_protocol ?? 'Request') : (record.provider ?? 'not routed');

  return html`
    <li>
      <a class="overview-feed-row" href=${href('/requests', { id: record.id })} data-fresh=${fresh ? '' : undefined}>
        <span class="overview-feed-lamp"><${StatusLamp} tone=${tone} pulse=${pending} title=${lampTitle} /></span>
        <span class="overview-feed-main">
          <span class="overview-feed-model mono" title=${model}>${model}</span>
          <span class="overview-feed-meta">
            <span class=${pending ? 'truncate' : 'mono truncate'} title=${first}>${first}</span>
            <span class="overview-feed-sep" aria-hidden="true">·</span>
            <span class="truncate" title=${key}>${key}</span>
          </span>
        </span>
        <span class="overview-feed-side">
          <span class="overview-feed-result">
            ${pending
              ? html`<${Badge} tone="info">${record.stream ? 'Streaming' : 'In flight'}<//>`
              : html`<${Badge} mono tone=${toneForStatus(record.status)}>${record.status}<//><span class="num">${formatDuration(record.duration_ms)}</span>`}
          </span>
          <span class="overview-feed-detail">
            ${pending
              ? html`started ${formatRelativeTime(record.started_at, now)}`
              : html`${record.ok || tokens > 0 ? `${formatTokens(tokens)} tokens` : (ERROR_KIND[kind] ?? (kind ? String(kind).replace(/_/g, ' ') : 'failed'))}
                  <span class="overview-feed-sep" aria-hidden="true">·</span>
                  ${formatRelativeTime(record.started_at, now)}`}
          </span>
        </span>
      </a>
    </li>
  `;
}

/** A request record, as the frame that would have put it in the list. */
const asEvent = (row) => ({ type: isPending(row) ? 'started' : 'finished', data: row });

/**
 * recent       /requests?limit=15 (useResource), polled by the page while
 *              the live connection is down
 * liveOpen     the live connection is up
 * reconnected  changes each time frames may have been missed (the connection
 *              came back, the gateway reported dropped frames), so the list
 *              is loaded again
 */
export default function Activity({ recent, liveOpen, reconnected }) {
  const now = useServerNow(1000);
  const [rows, setRows] = useState(() => (carried.length > 0 ? carried : null));
  const [fresh, setFresh] = useState(() => new Set());
  const [paused, setPaused] = useState(false);
  const [waiting, setWaiting] = useState(0);
  const [unlisted, setUnlisted] = useState(0);
  const queue = useRef([]);
  const timer = useRef(null);
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  const skipped = useRef(false);

  useEffect(() => {
    carried = (rows ?? []).filter(isPending);
  }, [rows]);

  // A fresh load is merged with what is on screen: rows from live frames may
  // be newer than the response, and a row still in flight stays unless the
  // response shows that it has ended (its "finished" frame was missed).
  useEffect(() => {
    if (!recent.data) return;
    if (pausedRef.current) {
      skipped.current = true;
      return;
    }
    const loaded = (recent.data.items ?? []).slice(0, FEED_CAP);
    const ended = new Set(loaded.map((row) => row.id));
    const kept = (rowsRef.current ?? []).filter((row) => !(isPending(row) && ended.has(row.id)));
    setRows(applyFeed(loaded, kept.map(asEvent), FEED_CAP));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recent.data]);

  const firstConnection = useRef(reconnected);
  useEffect(() => {
    if (reconnected === firstConnection.current) return;
    recent.refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reconnected]);

  const flush = () => {
    timer.current = null;
    const events = queue.current;
    if (events.length === 0) return;
    if (pausedRef.current) {
      // Count what a resume would bring in: requests not on screen yet.
      const known = new Set((rowsRef.current ?? []).map((row) => row.id));
      setWaiting(new Set(events.map((event) => event.data.id).filter((id) => !known.has(id))).size);
      return;
    }
    queue.current = [];
    const before = new Set((rowsRef.current ?? []).map((row) => row.id));
    const next = applyFeed(rowsRef.current ?? [], events, FEED_CAP);
    setFresh(new Set(next.filter((row) => !before.has(row.id)).map((row) => row.id)));
    setRows(next);
    setWaiting(0);
  };

  const enqueue = (type) => (data) => {
    if (!data || data.id == null) return;
    queue.current.push({ type, data });
    if (queue.current.length > QUEUE_CAP) queue.current.splice(0, queue.current.length - QUEUE_CAP);
    if (timer.current == null) timer.current = setTimeout(flush, FLUSH_MS);
  };
  useLive('request.started', enqueue('started'));
  useLive('request.finished', enqueue('finished'));
  useEffect(() => () => clearTimeout(timer.current), []);

  // ---- Requests in flight that are not in the list ---------------------------
  // The vitals count every request in flight; the list knows only those it
  // heard start. The difference is said in words.
  const pendingIds = () => {
    const ids = new Set((rowsRef.current ?? []).filter(isPending).map((row) => row.id));
    // Frames that have arrived and are not applied yet.
    for (const event of queue.current) {
      if (event.type === 'started') ids.add(event.data.id);
      else ids.delete(event.data.id);
    }
    return ids;
  };
  const lastGap = useRef(0);
  const idleFrames = useRef(0);
  useLive('stats', (data) => {
    if (pausedRef.current || typeof data?.in_flight !== 'number') return;
    const listed = pendingIds().size;
    // Two frames in a row, so a request that starts between a frame and the
    // row that shows it does not flash the note.
    const gap = Math.max(0, data.in_flight - listed);
    setUnlisted(Math.min(gap, lastGap.current));
    lastGap.current = gap;
    // Nothing is in flight, yet rows say otherwise: their end was missed
    // (the page was away, frames were dropped). Take them off and reload.
    idleFrames.current = data.in_flight === 0 && listed > 0 ? idleFrames.current + 1 : 0;
    if (idleFrames.current >= 2) {
      idleFrames.current = 0;
      const cutoff = (data.at ?? Date.now()) - SETTLE_MS;
      const current = rowsRef.current ?? [];
      const next = current.filter((row) => !(isPending(row) && (row.started_at ?? 0) < cutoff));
      if (next.length !== current.length) {
        setRows(next);
        recent.refresh();
      }
    }
  });
  // Without frames the count comes from the polled status, through the vitals.
  const polledInFlight = useStore(gauges, (s) => s.inFlight);
  const listedNow = (rows ?? []).filter(isPending).length;
  useEffect(() => {
    if (liveOpen) return;
    lastGap.current = 0;
    setUnlisted(polledInFlight == null ? 0 : Math.max(0, polledInFlight - listedNow));
  }, [liveOpen, polledInFlight, listedNow]);

  const resume = () => {
    pausedRef.current = false;
    setPaused(false);
    flush();
    // Without live frames nothing was queued, and a load that arrived while
    // paused was not applied: load what happened meanwhile.
    if (!liveOpen || skipped.current) recent.refresh();
    skipped.current = false;
    // "N new requests" removes itself: the focus goes to the pause button.
    focusSoon(() => focusTarget('feed-toggle'));
  };

  const list = rows ?? [];
  const loading = rows == null && recent.loading;
  const failed = rows == null && recent.error;
  const base = paused ? 'Paused' : liveOpen ? 'Requests as they start and finish' : isStale(recent) ? 'Latest requests' : `Latest requests, refreshed every ${FEED_POLL_MS / 1000}s`;
  const showUnlisted = !paused && unlisted > 0;

  return html`
    <${Panel}
      class="overview-activity"
      title="Live activity"
      description=${described(base, recent)}
      flush
      actions=${html`
        ${paused && waiting > 0 && html`<${Button} size="sm" variant="ghost" onClick=${resume}>${plural(waiting, 'new request')}<//>`}
        <${IconButton}
          icon=${paused ? 'play' : 'pause'}
          label=${paused ? 'Resume the feed' : 'Pause the feed'}
          size="sm"
          aria-pressed=${paused ? 'true' : 'false'}
          data-overview-focus="feed-toggle"
          onClick=${() => (paused ? resume() : setPaused(true))}
        />
      `}
      footer=${html`<span>Newest first, up to ${FEED_CAP}</span><a href=${href('/requests')}>All requests</a>`}
    >
      ${showUnlisted &&
      html`<p class="overview-feed-note" role="status">
        <${StatusLamp} tone="info" pulse label=${`${plural(unlisted, 'more request')} in flight`} />
        <span class="overview-feed-note-text">A request that started before this page was opened is listed when it ends.</span>
      </p>`}
      ${rows != null &&
      recent.error &&
      !recent.data &&
      html`<p class="overview-feed-note">
        <${StatusLamp} tone="caution" label="Earlier requests could not be loaded" />
        <${Button} size="sm" variant="ghost" icon="refresh" onClick=${recent.refresh}>Try again<//>
      </p>`}
      ${loading
        ? html`<div class="overview-feed-skeleton" aria-hidden="true">
            ${Array.from({ length: 6 }, (_, i) => html`<div class="overview-feed-skel" key=${i}><${Skeleton} width=${`${[58, 44, 66, 38, 52, 47][i]}%`} /><${Skeleton} width="30%" height="10px" /></div>`)}
          </div>`
        : failed
          ? html`<${ErrorState} compact title="Could not load recent requests" error=${recent.error} onRetry=${recent.refresh} />`
          : list.length === 0
            ? !showUnlisted &&
              html`<${EmptyState}
                compact
                icon="requests"
                title="No requests yet"
                description="Each request a client sends appears here while it runs and after it ends. Send one from the playground to see it."
                action=${html`<${Button} size="sm" icon="playground" href=${href('/playground')}>Open the playground<//>`}
              />`
            : html`<ol class="overview-feed" aria-label="Most recent requests, newest first">
                ${list.map((record) => html`<${Row} key=${record.id} record=${record} now=${now} fresh=${fresh.has(record.id)} />`)}
              </ol>`}
    <//>
  `;
}
