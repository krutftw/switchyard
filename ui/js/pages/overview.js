// Overview (#/overview): the page the operator keeps open. Top to bottom it
// answers three questions: is the gateway healthy (status strip, notices,
// provider health), what is it doing right now (vitals, traffic, live
// activity), and who is using it (top models and client keys).
//
// Data: /status, /providers, /usage/timeseries, /usage/summary and /requests
// through useResource; the live topics stats, request.started,
// request.finished, credential, config.reloaded and lagged on top of them.
// Without the live connection every resource falls back to polling, and when
// the gateway stops answering altogether the page says so instead of showing
// its last state as the current one.
//
// The sections live in ./overview/: status.js, vitals.js, providers.js,
// traffic.js, activity.js, top.js; model.js holds the logic, data.js the
// state they share, stale.js the words for data that could not be renewed.

import { html, useEffect, useRef, useState } from '../../vendor/preact-htm.js';
import { ErrorState, Page, Panel } from '../components/index.js';
import { loadStyles } from '../lib/dom.js';
import { useLocalStorage, useResource } from '../lib/hooks.js';
import { liveState, useLive } from '../lib/live.js';
import { useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import Activity, { FEED_CAP, FEED_POLL_MS } from './overview/activity.js';
import { lastFrameAt, liveAttempts, loadedAttempts, syncClock, tail } from './overview/data.js';
import { firstRun, pruneTail } from './overview/model.js';
import ProviderBoard from './overview/providers.js';
import { isStale } from './overview/stale.js';
import { FirstRun, Notices, PutAway, StatusStrip } from './overview/status.js';
import TopLists from './overview/top.js';
import Traffic from './overview/traffic.js';
import Vitals from './overview/vitals.js';

await loadStyles('pages/overview.css');

// Live frames are applied in batches, so a burst costs one render.
const TAIL_FLUSH_MS = 500;
const CREDENTIAL_FLUSH_MS = 300;
// Request records the provider board counts upstream attempts from. The
// gateway serves at most 500 a page.
const ATTEMPT_PAGE = 500;
// The top lists follow traffic: at once when they are empty (the first
// requests of a new installation), otherwise at most this often.
const TOP_FOLLOW_MS = 10_000;
const TOP_FIRST_MS = 1_000;

/** Merge the runtime part of a credential (a "credential" frame) into /providers. */
function patchCredentials(providers, patches) {
  if (!Array.isArray(providers)) return providers;
  return providers.map((provider) => {
    const mine = patches.get(provider.name);
    if (!mine) return provider;
    return {
      ...provider,
      credentials: (provider.credentials ?? []).map((cred) => {
        const patch = mine.get(cred.id);
        if (!patch) return cred;
        // Frames omit values that are absent; in the view those are null.
        return { ...cred, cooldown_until: null, cooldown_reason: null, unusable_reason: null, ...patch };
      }),
    };
  });
}

export default function Overview() {
  const live = useStore(liveState);
  const liveOpen = live.status === 'open';
  const [rangeParam, setRange] = useQueryParam('range', '1h');
  const range = rangeParam === '24h' ? '24h' : '1h';

  // Slow refreshes behind the live frames; brisk ones when there are none.
  const status = useResource('/status', { pollMs: liveOpen ? 30_000 : 5_000 });
  const providers = useResource('/providers', { pollMs: liveOpen ? 30_000 : 10_000 });
  const hour = useResource(['/usage/timeseries', { range: '1h' }], { pollMs: liveOpen ? 60_000 : 15_000 });
  const day = useResource(range === '24h' ? ['/usage/timeseries', { range: '24h' }] : null, { pollMs: 60_000 });
  const top = useResource(['/usage/summary', { range: '24h' }], { pollMs: liveOpen ? 60_000 : 30_000 });
  // The feed's first page. Live frames keep it current; it is polled while
  // there are none, and until it has loaded once.
  const [feedLoaded, setFeedLoaded] = useState(false);
  const feed = useResource(['/requests', { limit: FEED_CAP }], { pollMs: liveOpen && feedLoaded ? 0 : FEED_POLL_MS });
  // Request records with their upstream attempts, for the provider board.
  // Live frames extend them; the reload is a safety net.
  const records = useResource(['/requests', { limit: ATTEMPT_PAGE }], { pollMs: liveOpen ? 300_000 : 30_000 });

  const [dismissed, setDismissed] = useLocalStorage('overview.dismissedWarnings', []);
  const [setupHidden, setSetupHidden] = useLocalStorage('overview.setupHidden', false);

  const refreshAll = () => {
    status.refresh();
    providers.refresh();
    hour.refresh();
    day.refresh();
    top.refresh();
  };

  // ---- Clock ---------------------------------------------------------------
  useEffect(() => {
    const data = status.data;
    if (data?.started_at != null && data?.uptime_ms != null) syncClock(data.started_at + data.uptime_ms);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status.updatedAt]);

  useEffect(() => {
    if (feed.data) setFeedLoaded(true);
  }, [feed.data]);

  useEffect(() => {
    if (records.data) loadedAttempts(records.data);
  }, [records.data]);

  // ---- Finished requests extend the chart, the board and the top lists -------
  const finished = useRef([]);
  const tailTimer = useRef(null);
  const topTimer = useRef(null);
  const topRef = useRef(top);
  topRef.current = top;
  const statusRef = useRef(status);
  statusRef.current = status;
  useLive('request.finished', (record) => {
    if (!record) return;
    finished.current.push(record);
    if (tailTimer.current != null) return;
    tailTimer.current = setTimeout(() => {
      tailTimer.current = null;
      const added = finished.current;
      finished.current = [];
      tail.replace(pruneTail([...tail.get(), ...added.map((r) => ({ at: r.finished_at ?? r.started_at ?? Date.now(), ok: r.ok === true }))], null));
      liveAttempts(added);
      // The first request of an installation ticks off a setup step, which
      // reads the totals in /status: do not make it wait for the next poll.
      if ((statusRef.current.data?.live?.totals?.requests ?? 0) === 0) statusRef.current.refresh();
      if (topTimer.current == null) {
        const empty = (topRef.current.data?.totals?.requests ?? 0) === 0;
        topTimer.current = setTimeout(
          () => {
            topTimer.current = null;
            topRef.current.refresh();
          },
          empty ? TOP_FIRST_MS : TOP_FOLLOW_MS,
        );
      }
    }, TAIL_FLUSH_MS);
  });

  // What a loaded series already counts is dropped from the tail.
  useEffect(() => {
    const loaded = [hour.data?.to, range === '24h' ? day.data?.to : null].filter((to) => typeof to === 'number');
    if (loaded.length === 0) return;
    tail.replace(pruneTail(tail.get(), Math.min(...loaded)));
  }, [hour.data, day.data, range]);

  // ---- Credential frames move the board at once ------------------------------
  const patches = useRef(new Map());
  const patchTimer = useRef(null);
  useLive('credential', (data) => {
    if (!data?.provider || !data.credential?.id) return;
    let mine = patches.current.get(data.provider);
    if (!mine) patches.current.set(data.provider, (mine = new Map()));
    mine.set(data.credential.id, data.credential);
    if (patchTimer.current != null) return;
    patchTimer.current = setTimeout(() => {
      patchTimer.current = null;
      const batch = patches.current;
      patches.current = new Map();
      providers.mutate((list) => patchCredentials(list, batch));
    }, CREDENTIAL_FLUSH_MS);
  });

  useEffect(
    () => () => {
      clearTimeout(tailTimer.current);
      clearTimeout(patchTimer.current);
      clearTimeout(topTimer.current);
      tail.replace([]);
    },
    [],
  );

  // ---- Configuration changes, reconnects, dropped frames ---------------------
  useLive('config.reloaded', refreshAll);

  // Frames sent while the connection was down are gone: load again when it
  // comes back. The first connection after the page opened needs nothing.
  // When it goes down, ask for the status at once: whether the gateway still
  // answers is the first thing the page has to know.
  const [connections, setConnections] = useState(0);
  const wasOpen = useRef(liveOpen);
  const everOpen = useRef(liveOpen);
  const catchUp = () => {
    refreshAll();
    records.refresh();
    setConnections((n) => n + 1);
  };
  useEffect(() => {
    if (liveOpen && !wasOpen.current) {
      if (everOpen.current) catchUp();
      everOpen.current = true;
    }
    if (!liveOpen && wasOpen.current) status.refresh();
    wasOpen.current = liveOpen;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [liveOpen]);
  // The gateway dropped frames for this connection: the same remedy.
  useLive('lagged', catchUp);

  // ---- Render ----------------------------------------------------------------
  const description = 'Whether the gateway is healthy, what it is doing right now, and what needs attention.';

  // Without /status the page cannot answer its first question. One message,
  // not one per section.
  if (status.error && !status.data) {
    return html`
      <${Page} title="Overview" description=${description} class="overview">
        <${Panel} flush>
          <${ErrorState} title="Could not load the gateway status" error=${status.error} onRetry=${refreshAll} retrying=${status.loading} />
        <//>
      <//>
    `;
  }

  // No live frames and no answer to /status: the gateway is not there. What
  // is on screen is then history, and every section is told.
  const down = !liveOpen && isStale(status) ? { at: Math.max(status.updatedAt ?? 0, lastFrameAt() ?? 0), unreachable: status.error.status === 0 } : null;

  // What could not be renewed (old data stays on screen), and the request
  // records when they never arrived (the board is then on screen with no
  // attempts to show; without the board there is nothing to explain).
  const sections = [
    { what: 'the status', resource: status },
    { what: 'provider health', resource: providers },
    { what: 'provider attempts', resource: records },
    { what: 'the traffic chart', resource: range === '24h' ? day : hour },
    { what: 'the top lists', resource: top },
    { what: 'recent requests', resource: feed },
  ].filter((section) => isStale(section.resource) || (section.resource === records && Boolean(records.error) && providers.data != null));
  const retry = () => {
    for (const section of sections) section.resource.refresh();
  };

  const setup = firstRun(status.data, providers.data);
  const hiddenWarnings = (status.data?.warnings ?? []).filter((w) => dismissed.includes(w)).length;

  return html`
    <${Page} title="Overview" description=${description} class="overview">
      <${StatusStrip} status=${status} providers=${providers} down=${down} />
      <${Notices} status=${status} down=${down} sections=${sections} onRetry=${retry} dismissed=${dismissed} setDismissed=${setDismissed} />
      ${setup.show && !setupHidden && html`<${FirstRun} status=${status} providers=${providers} onHide=${() => setSetupHidden(true)} />`}
      <${PutAway} warnings=${hiddenWarnings} setup=${setup.show && setupHidden} onWarnings=${() => setDismissed([])} onSetup=${() => setSetupHidden(false)} />
      <${Vitals} status=${status} liveStatus=${live.status} down=${down} hour=${hour} />
      <${ProviderBoard} providers=${providers} recent=${records} onExpire=${providers.refresh} />
      <div class="overview-cols">
        <${Traffic} series=${range === '24h' ? day : hour} range=${range} onRange=${setRange} />
        <${TopLists} summary=${top} />
        <${Activity} recent=${feed} liveOpen=${liveOpen} reconnected=${connections} />
      </div>
    <//>
  `;
}
