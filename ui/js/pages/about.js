// About (#/about): what is running, how to point a client at it, and the
// reference an operator keeps coming back to (endpoints, reasoning suffixes,
// keyboard shortcuts, licences).
//
// Data: GET /status and GET /models (the models the examples may use), both
// kept fresh by the config.reloaded live topic, by polling while the live
// connection is down, and refetched when live frames may have been missed;
// and the licence texts the gateway serves next to the dashboard. Nothing on
// this page writes.
//
// URL state: ?section= (jump target), ?client= (tab in "Connect a client"),
// ?model= (model in the examples), ?addr= (which base URL the examples use).
// The shell choice is a preference of this device and lives in localStorage.
// A link can therefore put text into commands that get pasted into a
// terminal: ?model= is used only when GET /models confirms the name, and
// about/snippets.js quotes every value for the shell it is written for.
//
// The words live in about/reference.js (static reference) and
// about/snippets.js (addresses, client snippets, diagnostics text).

import { html, useEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  CodeBlock,
  CopyButton,
  ErrorState,
  Field,
  Icon,
  Kbd,
  KeyValue,
  Notice,
  Page,
  Panel,
  Segmented,
  Select,
  Skeleton,
  StatusLamp,
  Table,
  Tabs,
  toast,
} from '../components/index.js';
import { ApiError } from '../lib/api.js';
import { useCommands } from '../lib/commands.js';
import { copyText, loadStyles, prefersReducedMotion } from '../lib/dom.js';
import { formatDateTime, formatDuration, formatNumber, formatTime, plural, sentence } from '../lib/format.js';
import { hotkeyLabel, useIsPhone, useLocalStorage, useNow, useResource } from '../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../lib/live.js';
import { href, routeStore, setQuery, useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import {
  ARROWS,
  CLIPROXY_URL,
  EFFORT_BUDGETS,
  ENDPOINTS,
  KEY_PLACES,
  LICENCE_FILES,
  MIT_LICENCE,
  REASONING,
  REASONING_RULES,
  REPOSITORY_URL,
  SHORTCUTS,
  endpointUrl,
} from './about/reference.js';
import {
  CLIENTS,
  KEY_PLACEHOLDER,
  MODEL_PLACEHOLDER,
  SHELLS,
  addressChoices,
  callableModels,
  clientSnippets,
  defaultModel,
  diagnosticsText,
  modelReady,
  parseLicenceSections,
  parseListen,
  servesModel,
} from './about/snippets.js';

await loadStyles('pages/about.css');

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

const SECTIONS = [
  { id: 'gateway', label: 'Gateway' },
  { id: 'connect', label: 'Connect a client' },
  { id: 'endpoints', label: 'Endpoints' },
  { id: 'reasoning', label: 'Reasoning suffix' },
  { id: 'shortcuts', label: 'Shortcuts' },
  { id: 'licence', label: 'Licence' },
];

const sectionElement = (id) => document.getElementById(`about-${id}`);

/**
 * Bring a section to the top. A jump the user asked for glides (unless they
 * turned motion off); arriving by a link does not: `instant` places the page.
 */
function scrollToSection(id, instant = false) {
  sectionElement(id)?.scrollIntoView({ behavior: instant || prefersReducedMotion() ? 'auto' : 'smooth', block: 'start' });
}

/**
 * Move keyboard focus to a section (its panel takes focus, it is not a tab
 * stop), so the next Tab goes into the section and a screen reader names it
 * (a panel is a region named by its title). Scrolling is left to
 * scrollToSection.
 */
function focusSection(id) {
  sectionElement(id)?.focus({ preventScroll: true });
}

/**
 * Set ?section= (and whatever else) without a history entry, then go there:
 * the page scrolls and keyboard focus follows. Also right for a command run
 * from the palette: by the time it runs, the palette has closed and handed
 * the focus back, so the focus placed here stays.
 */
function jumpTo(id, extra) {
  setQuery({ ...extra, section: id });
  // The sections do not move when a query parameter changes, so there is
  // nothing to wait for.
  scrollToSection(id);
  focusSection(id);
}

/**
 * A "Try again" that works takes its error state, and so itself, off the
 * page, which would leave the keyboard on <body>. This returns the handler
 * for such a button: once it has been pressed and `failed` turns false,
 * focus goes to what `target()` returns, unless the reader has put it
 * somewhere else meanwhile.
 */
function useRetryFocus(failed, retry, target) {
  const pressed = useRef(false);
  const targetRef = useRef(target);
  targetRef.current = target;
  useEffect(() => {
    if (failed || !pressed.current) return;
    pressed.current = false;
    const active = document.activeElement;
    if (!active || active === document.body) targetRef.current()?.focus({ preventScroll: true });
  }, [failed]);
  return () => {
    pressed.current = true;
    return retry();
  };
}

function SectionNav({ query }) {
  return html`
    <nav class="about-nav" aria-label="Sections of this page">
      ${SECTIONS.map(
        (section) => html`
          <a
            key=${section.id}
            class="about-nav-item"
            href=${href('/about', { ...query, section: section.id })}
            onClick=${(event) => {
              // A modified click opens the link the browser's way.
              if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
              event.preventDefault();
              jumpTo(section.id);
            }}
          >
            ${section.label}
          </a>
        `,
      )}
    </nav>
  `;
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

/** The uptime, ticking. Its own component so only this text re-renders. */
function Uptime({ uptimeMs, at, startedAt }) {
  const now = useNow(1000);
  const elapsed = at ? Math.max(0, now - at) : 0;
  return html`<span class="num">${formatDuration(uptimeMs + elapsed)}</span> <span class="faint">since ${formatDateTime(startedAt)}</span>`;
}

function GatewayPanel({ status, liveOpen }) {
  const data = status.data;
  const failed = !!status.error && !data;
  const retry = useRetryFocus(failed, status.refresh, () => sectionElement('gateway'));
  const blank = (width) => html`<${Skeleton} width=${width} />`;
  const warnings = data?.warnings ?? [];

  const items = [
    { label: 'Version', value: data ? data.version : blank('48px'), mono: true, copy: true },
    {
      label: 'Uptime',
      value: data ? html`<${Uptime} uptimeMs=${data.uptime_ms} at=${status.updatedAt} startedAt=${data.started_at} />` : blank('160px'),
    },
    { label: 'Listening on', value: data ? data.listen : blank('120px'), mono: true, copy: true },
    { label: 'Config file', value: data ? data.config_path : blank('240px'), mono: true, copy: true },
    { label: 'Data directory', value: data ? data.data_dir : blank('200px'), mono: true, copy: true },
    {
      label: 'Client keys',
      value: !data
        ? blank('96px')
        : data.auth_required
          ? html`<${StatusLamp} tone="clear" label="Required" />`
          : html`<${StatusLamp} tone="caution" label="Not required" detail="anyone who can reach the gateway can use it" class="about-status-wrap" />`,
    },
    {
      label: 'Admin access',
      value: !data
        ? blank('140px')
        : data.admin?.allow_remote
          ? `Other machines are allowed${data.admin.remote ? '; you are connected from one' : ''}`
          : 'This machine only',
    },
    {
      label: 'Warnings',
      value: !data
        ? blank('64px')
        : warnings.length === 0
          ? 'None'
          : html`<${StatusLamp} tone="caution" label=${formatNumber(warnings.length)} detail="listed in the diagnostics" />`,
    },
  ];

  return html`
    <${Panel}
      id="about-gateway"
      tabindex="-1"
      class="about-section"
      title="Gateway"
      description="The process this dashboard is talking to."
      footer=${failed
        ? null
        : html`
            <span>${data ? `Checked at ${formatTime(status.updatedAt)}. ${liveOpen ? 'Refreshes when the configuration changes.' : 'Live updates are off, so this refreshes every 15 seconds.'}` : 'Asking the gateway.'}</span>
            <${Button} size="sm" icon="refresh" loading=${status.refreshing} disabled=${!data} onClick=${status.refresh}>Refresh<//>
          `}
    >
      ${failed
        ? html`<${ErrorState} compact title="Could not load the gateway status" error=${status.error} onRetry=${retry} retrying=${status.loading} />`
        : html`<div aria-busy=${data ? undefined : 'true'}><${KeyValue} items=${items} /></div>`}
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

function DiagnosticsPanel({ status, textRef }) {
  // The text changes once a minute, not once a second: a block that rewrites
  // itself while someone is selecting it is a block nobody can copy by hand.
  const minute = useNow(60_000);
  const data = status.data;
  const text = useMemo(() => {
    let uptimeMs = null;
    if (data) {
      const running = data.uptime_ms + Math.max(0, minute - (status.updatedAt ?? minute));
      uptimeMs = running < 60_000 ? data.uptime_ms : Math.floor(running / 60_000) * 60_000;
    }
    return diagnosticsText(data, { uptimeMs, error: status.error });
  }, [data, status.error, status.updatedAt, minute]);
  textRef.current = text;
  // Pending only until the first attempt has an answer. A retry or a poll
  // after a failure keeps the "unavailable" text and the copy button: that
  // text is what someone needs when the gateway cannot be reached.
  const pending = status.loading && !data && !status.error;

  return html`
    <${Panel}
      id="about-diagnostics"
      tabindex="-1"
      class="about-section"
      title="Diagnostics"
      description="Paste this when you ask for help. It has no keys, paths or addresses."
      actions=${html`<${CopyButton} variant="secondary" value=${text} label="Copy diagnostics" disabled=${pending}>Copy<//>`}
    >
      ${pending
        ? html`<div class="about-code-skel" aria-busy="true" aria-label="Loading diagnostics"><${Skeleton} lines=${8} /></div>`
        : html`<${CodeBlock}
            language="text"
            title="Plain text"
            value=${text}
            copy=${false}
            wrap
            maxHeight="340px"
            note=${data?.warnings?.length ? 'Warnings name providers, aliases and environment variables. Read them before posting in public.' : undefined}
          />`}
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Connect a client
// ---------------------------------------------------------------------------

/** Text from a link, cut to a length that fits a notice. */
function clip(text, max = 96) {
  const value = String(text);
  return value.length > max ? `${value.slice(0, max)}…` : value;
}

function guessShell() {
  if (typeof navigator === 'undefined') return 'posix';
  return /Win/i.test(navigator.platform || navigator.userAgent || '') ? 'powershell' : 'posix';
}

function ConnectPanel({ status, models }) {
  const [clientParam] = useQueryParam('client', CLIENTS[0].id);
  const [modelParam] = useQueryParam('model', '');
  const [addrParam] = useQueryParam('addr', 'browser');
  // A choice made here also names this section in the URL, so the link that
  // reproduces the snippets lands on them. Defaults stay out of the URL.
  const choose = (key, value, fallbackValue) => setQuery({ [key]: value === fallbackValue ? null : value, section: 'connect' });
  const setClient = (value) => choose('client', value, CLIENTS[0].id);
  const setAddr = (value) => choose('addr', value, 'browser');
  const [shellPref, setShell] = useLocalStorage('about.shell', null);

  const client = CLIENTS.some((c) => c.id === clientParam) ? clientParam : CLIENTS[0].id;
  const shell = SHELLS.some((s) => s.value === shellPref) ? shellPref : guessShell();

  const listen = status.data?.listen;
  // GET /status says whether the listener serves HTTPS: the scheme in front
  // of the listen address is a fact, not a guess.
  const tls = status.data?.tls === true;
  const choices = useMemo(() => addressChoices(listen, document.baseURI, { tls }), [listen, tls]);
  const address = choices.find((choice) => choice.id === addrParam) ?? choices[0];
  const bound = parseListen(listen);

  // The picker offers what a client can call: listed names the gateway can
  // route (an alias without a routable target is `ignored` and left out). A
  // name whose credentials all rest is kept, and says so.
  const picks = useMemo(
    () =>
      callableModels(models.data)
        .filter((m) => !m.hidden)
        .map((m) => ({ value: m.name, label: modelReady(m) ? m.name : `${m.name}, no credential ready` })),
    [models.data],
  );
  const fallback = useMemo(() => defaultModel(models.data), [models.data]);
  // ?model= is text from a link, and the snippets are commands somebody will
  // paste into a terminal. It is used only once the gateway's own list
  // confirms it: a served name, with or without a reasoning suffix. Until
  // the list has loaded, and when it says no, the examples use the default.
  const loaded = Array.isArray(models.data);
  const asked = modelParam !== '';
  const accepted = useMemo(() => asked && servesModel(models.data, modelParam), [asked, modelParam, models.data]);
  const model = accepted ? modelParam : fallback;
  const refused = asked && loaded && !accepted && picks.length > 0;
  // An accepted name that is not in the picker (a hidden name, a name with a
  // suffix) is shown in it, so the control says what the examples use.
  const options = model && !picks.some((pick) => pick.value === model) ? [{ value: model, label: model }, ...picks] : picks;
  const noModels = loaded && picks.length === 0 && !accepted;
  const dismissRefused = () => {
    setQuery({ model: null });
    // The notice and its button go away: leave focus on the control it named.
    document.getElementById('about-model')?.focus({ preventScroll: true });
  };

  const snippets = useMemo(() => clientSnippets(client, { base: address.base, model, shell }), [client, address.base, model, shell]);
  const authRequired = status.data?.auth_required;
  // "Try again" under the controls goes away once the list has loaded: the
  // picker took its place (or, with no model to pick, the panel).
  const retryModels = useRetryFocus(!!models.error && !models.data, models.refresh, () => document.getElementById('about-model') ?? sectionElement('connect'));

  return html`
    <${Panel} id="about-connect" tabindex="-1" class="about-section" title="Connect a client" description="Point a client at the gateway and give it a client key. The snippets use this gateway’s address.">
      <div class="stack">
        <div class="about-controls">
          ${choices.length > 1 &&
          html`
            <${Field} label="Address" hint=${address.id === 'listen' ? `What a client on the gateway’s own machine uses. The gateway serves ${tls ? 'HTTPS' : 'plain HTTP'} there.` : 'How this browser reaches the gateway.'}>
              <${Segmented} label="Address in the snippets" size="sm" value=${address.id} onChange=${setAddr} options=${choices.map((choice) => ({ value: choice.id, label: choice.label }))} />
            <//>
          `}
          <${Field} label="Shell">
            <${Segmented} label="Shell the snippets are written for" size="sm" value=${shell} onChange=${setShell} options=${SHELLS} />
          <//>
          ${options.length > 0
            ? html`<${Select} id="about-model" class="about-model" label="Model in the examples" size="sm" value=${model ?? ''} onChange=${(value) => choose('model', value, fallback)} options=${options} />`
            : models.loading
              ? html`<${Field} label="Model in the examples"><${Skeleton} width="180px" height="28px" /><//>`
              : null}
        </div>

        ${bound?.wildcard &&
        html`<p class="about-aside">The gateway listens on every interface (<span class="mono">${listen}</span>). From another machine, replace the host in the snippets with this machine’s name or address.</p>`}
        ${models.error && !models.data && html`<p class="about-aside">The model list did not load, so the examples use the placeholder <span class="mono">${MODEL_PLACEHOLDER}</span>. <button type="button" class="about-link" onClick=${retryModels}>Try again</button></p>`}
        ${noModels &&
        html`
          <${Notice} tone="caution" title="No models to call yet" action=${html`<${Button} size="sm" href=${href('/providers')}>Open providers<//>`}>
            The gateway serves no model, so the examples use the placeholder <span class="mono">${MODEL_PLACEHOLDER}</span>. Add a provider and its models appear here.
          <//>
        `}
        ${refused &&
        html`
          <${Notice} tone="caution" title="The model in the link is not served here" action=${html`<${Button} size="sm" onClick=${dismissRefused}>Dismiss<//>`}>
            This gateway serves no model named <span class="mono about-break">${clip(modelParam)}</span>, so the examples use <span class="mono about-break">${model ?? MODEL_PLACEHOLDER}</span>. Pick another under “Model in the examples”.
          <//>
        `}

        <${Tabs} label="Client" value=${client} onChange=${setClient} tabs=${CLIENTS} />

        <div class="stack" role="tabpanel" aria-label=${CLIENTS.find((c) => c.id === client).label} style="--gap:var(--space-3)">
          <p class="about-lead">${snippets.lead}</p>
          <div class="about-blocks">
            ${snippets.blocks.map((block) => html`<${CodeBlock} key=${`${client}-${block.id}`} language="text" title=${block.title} value=${block.code} note=${block.note} maxHeight="300px" />`)}
          </div>
        </div>

        <${Notice}
          icon="key"
          title=${authRequired === false ? 'This gateway does not ask for a key' : `Replace ${KEY_PLACEHOLDER} with a client key`}
          action=${html`<${Button} size="sm" href=${href('/keys')}>Open API keys<//>`}
        >
          ${authRequired === false
            ? 'Requests are accepted without a client key (auth.required is off). A client that insists on a key can send any value.'
            : 'Create a key, or reveal one you already have, under API keys. Keys are never shown on this page.'}
        <//>
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/** A path that may break after a slash or a colon, and nowhere else. */
function BreakablePath({ path }) {
  const parts = path.split(/(?<=[/:?])/);
  return html`<span class="mono about-path-text">${parts.map((part, i) => html`${i > 0 && html`<wbr />`}${part}`)}</span>`;
}

function EndpointsPanel({ base }) {
  const columns = useMemo(
    () => [
      {
        key: 'path',
        header: 'Endpoint',
        primary: true,
        width: '40%',
        render: (endpoint) => html`
          <div class="about-path">
            <span class="about-badges">
              ${endpoint.ws ? html`<${Badge} mono tone="info" title="WebSocket upgrade (a GET request)">WS<//>` : html`<${Badge} mono outline>${endpoint.method}<//>`}
            </span>
            <div class="about-path-list">
              ${endpoint.paths.map((path) => html`<${BreakablePath} key=${path} path=${path} />`)}
              <span class="about-path-family">${endpoint.family}</span>
            </div>
            <${CopyButton} value=${endpointUrl(endpoint, base)} label=${`Copy the ${endpoint.ws ? 'WebSocket ' : ''}URL of ${endpoint.paths[0]}`} />
          </div>
        `,
      },
      { key: 'family', header: 'API', width: '104px' },
      {
        key: 'text',
        header: 'What it does',
        render: (endpoint) => html`<span class="about-cell-text">${endpoint.text}${endpoint.open && html` <${Badge} outline>No key needed<//>`}</span>`,
      },
    ],
    [base],
  );

  return html`
    <${Panel}
      id="about-endpoints"
      tabindex="-1"
      class="about-section about-endpoints"
      title="Client API endpoints"
      description="Everything a client can call. The copy button gives the full URL on this gateway."
      flush
      footer=${html`<span class="about-foot-text">A client key goes in ${KEY_PLACES.map((place, i) => html`${i > 0 && (i === KEY_PLACES.length - 1 ? ' or ' : ', ')}<span class="mono" key=${place}>${place}</span>`)}. The dashboard and its API are under <span class="mono">/admin/</span>.</span>`}
    >
      <${Table} columns=${columns} rows=${ENDPOINTS} rowKey="id" sticky=${false} caption="Client API endpoints" />
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Reasoning suffix
// ---------------------------------------------------------------------------

const FAMILIES = [
  { key: 'openai', label: 'OpenAI' },
  { key: 'anthropic', label: 'Anthropic' },
  { key: 'gemini', label: 'Gemini' },
];

/**
 * One suffix and what it becomes for each provider family.
 *
 * Not the shared Table: every cell is a few lines of code and a sentence,
 * and between the phone layout and a wide desktop five such columns do not
 * fit. This grid folds its three provider cells as the room runs out.
 */
function SuffixRow({ row }) {
  return html`
    <section class="about-suffix-row" aria-label=${row.suffix}>
      <div class="about-suffix-head">
        <h3 class="about-suffix"><code>${row.suffix}</code></h3>
        <p class="about-suffix-meaning">${row.meaning}</p>
      </div>
      <dl class="about-suffix-cells">
        ${FAMILIES.map(({ key, label }) => {
          // One case, or one per kind of model when the result depends on it.
          const cases = [].concat(row[key]);
          return html`
            <div class="about-reason" key=${key}>
              <dt class="plate-label">${label}</dt>
              <dd class="about-reason-body">
                ${cases.map(
                  (part, i) => html`
                    <div class="about-reason-case" key=${i}>
                      ${part.code && part.code.map((line) => html`<code class="about-reason-code" key=${line}>${line}</code>`)}
                      ${part.note && html`<span class=${part.code || cases.length > 1 ? 'about-reason-note' : 'about-reason-plain'}>${part.note}</span>`}
                    </div>
                  `,
                )}
              </dd>
            </div>
          `;
        })}
      </dl>
    </section>
  `;
}

function ReasoningPanel() {
  return html`
    <${Panel}
      id="about-reasoning"
      tabindex="-1"
      class="about-section about-reasoning"
      title="Reasoning suffix"
      description="Add a suffix to any model name to set how hard the model thinks. The gateway writes it in the form the serving provider understands."
      flush
    >
      <div class="about-suffixes">${REASONING.map((row) => html`<${SuffixRow} key=${row.suffix} row=${row} />`)}</div>
      <div class="about-pad about-reason-foot">
        <div class="stack" style="--gap:var(--space-2)">
          <h3>Levels and budgets</h3>
          <p class="about-aside">When a level has to become a budget, it becomes this many tokens. Going the other way, a budget takes the first level that covers it; budgets above 24,576 count as xhigh.</p>
          <ul class="about-levels" aria-label="Token budget of each effort level">
            ${EFFORT_BUDGETS.map(
              (entry) => html`
                <li key=${entry.level}>
                  <span class="mono">${entry.level}</span>
                  <span class="num">${formatNumber(entry.budget)}</span>
                </li>
              `,
            )}
          </ul>
        </div>
        <div class="stack" style="--gap:var(--space-2)">
          <h3>Rules</h3>
          <ul class="about-rules">
            ${REASONING_RULES.map((rule) => html`<li key=${rule}>${rule}</li>`)}
          </ul>
        </div>
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Keyboard shortcuts
// ---------------------------------------------------------------------------

function KeyCap({ name }) {
  const arrow = ARROWS[name];
  if (arrow) {
    return html`<${Kbd}><${Icon} name=${arrow.icon} size=${10} /><span class="sr-only">${arrow.label}</span><//>`;
  }
  return html`<${Kbd}>${hotkeyLabel(name)}<//>`;
}

function ShortcutKeys({ keys }) {
  return html`
    <span class="about-keys">
      ${keys.map(
        (chord, i) => html`
          ${i > 0 && html`<span class="about-keys-or" key=${`or-${i}`}>or</span>`}
          <span class="about-chord" key=${chord.join('+')}>${chord.map((name) => html`<${KeyCap} key=${name} name=${name} />`)}</span>
        `,
      )}
    </span>
  `;
}

function ShortcutsPanel() {
  const isPhone = useIsPhone();
  return html`
    <${Panel} id="about-shortcuts" tabindex="-1" class="about-section" title="Keyboard shortcuts" description="Everything in the dashboard works from the keyboard.">
      <div class="about-shortcuts">
        ${SHORTCUTS.map((group) => {
          const items = group.items.filter((item) => !(item.desktop && isPhone));
          if (items.length === 0) return null;
          return html`
            <section key=${group.title} class="about-shortcut-group" aria-label=${group.title}>
              <h3 class="plate-label">${group.title}</h3>
              <dl class="about-shortcut-list">
                ${items.map(
                  (item) => html`
                    <div class="about-shortcut" key=${item.action}>
                      <dt><${ShortcutKeys} keys=${item.keys} /></dt>
                      <dd>${item.action}</dd>
                    </div>
                  `,
                )}
              </dl>
            </section>
          `;
        })}
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Licence and acknowledgements
// ---------------------------------------------------------------------------

/**
 * A text file the gateway serves next to the dashboard (not under the admin
 * API, so no secret is sent). Settles in every case, as useResource asks.
 */
async function fetchText(path, signal) {
  const controller = new AbortController();
  const onAbort = () => controller.abort();
  if (signal?.aborted) controller.abort();
  else signal?.addEventListener('abort', onAbort, { once: true });
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, 15_000);
  try {
    const res = await fetch(new URL(path, document.baseURI), { signal: controller.signal, cache: 'no-cache', credentials: 'same-origin' });
    if (!res.ok) {
      const what = res.status === 404 ? `The gateway does not serve ${path}.` : `The gateway answered HTTP ${res.status} for ${path}.`;
      throw new ApiError(res.status, `${what} The same text is in the source repository.`);
    }
    return await res.text();
  } catch (cause) {
    if (cause instanceof ApiError) throw cause;
    if (timedOut) throw new ApiError(0, `${path} did not arrive within 15 seconds. Check the connection to the gateway and try again.`, { code: 'timeout' });
    if (signal?.aborted) throw new ApiError(0, 'The request was cancelled.', { code: 'aborted' });
    throw new ApiError(0, `Could not reach the gateway to load ${path}. Check that Switchyard is running and try again.`, { code: 'network' });
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', onAbort);
  }
}

/** A native disclosure: keyboard and screen-reader behaviour come with it. */
function Disclosure({ title, hint, onOpen, children }) {
  const [open, setOpen] = useState(false);
  return html`
    <details
      class="about-details"
      onToggle=${(event) => {
        const now = event.currentTarget.open;
        setOpen(now);
        if (now) onOpen?.();
      }}
    >
      <summary class="about-summary">
        <${Icon} name="chevron-right" size=${14} class="about-summary-mark" />
        <span class="about-summary-title">${title}</span>
        ${hint && html`<span class="about-summary-hint">${hint}</span>`}
      </summary>
      ${open && html`<div class="about-details-body">${children}</div>`}
    </details>
  `;
}

function LicenceText({ resource, path }) {
  const host = useRef(null);
  // When a retry works the text replaces the error state: the keyboard goes
  // to the heading of the disclosure it is in.
  const retry = useRetryFocus(!!resource.error && resource.data == null, resource.refresh, () => host.current?.closest('details')?.querySelector('summary'));
  return html`
    <div ref=${host}>
      ${resource.data != null
        ? html`<${CodeBlock} language="text" title=${path} value=${resource.data} maxHeight="320px" />`
        : resource.error
          ? html`<${ErrorState} compact title="Could not load the licence text" error=${resource.error} onRetry=${retry} retrying=${resource.loading} />`
          : html`<div class="about-code-skel" aria-busy="true" aria-label="Loading the licence text"><${Skeleton} lines=${6} /></div>`}
    </div>
  `;
}

/** A licence file fetched the first time its disclosure is opened. */
function LazyLicence({ file }) {
  const [wanted, setWanted] = useState(false);
  const resource = useResource((signal) => fetchText(file.path, signal), { enabled: wanted, deps: [file.path] });
  return html`
    <${Disclosure} title=${file.title} hint=${file.path} onOpen=${() => setWanted(true)}>
      <${LicenceText} resource=${resource} path=${file.path} />
    <//>
  `;
}

function LicencePanel() {
  const [vendorFile, ...fontFiles] = LICENCE_FILES;
  // Loaded with the page: the list of bundled pieces is read from it.
  const vendor = useResource((signal) => fetchText(vendorFile.path, signal));
  const bundled = useMemo(() => parseLicenceSections(vendor.data), [vendor.data]);

  return html`
    <${Panel} id="about-licence" tabindex="-1" class="about-section" title="Licence and acknowledgements">
      <div class="stack">
        <div class="stack about-prose" style="--gap:var(--space-2)">
          <p>Switchyard is free software under the <strong>MIT licence</strong>. The source, the issue tracker and the releases are at <a href=${REPOSITORY_URL} target="_blank" rel="noopener noreferrer">github.com/krutftw/switchyard</a>.</p>
          <p>It is an independent implementation inspired by <a href=${CLIPROXY_URL} target="_blank" rel="noopener noreferrer">CLIProxyAPI</a>. Its cooldown rules, its reasoning conversion tables and its model catalog were informed by that project.</p>
        </div>

        <div class="stack" style="--gap:var(--space-2)">
          <h3>Bundled with the dashboard</h3>
          ${vendor.loading && !vendor.data
            ? html`<div aria-busy="true" aria-label="Loading the list of bundled software"><${Skeleton} lines=${3} /></div>`
            : bundled.length > 0
              ? html`
                  <ul class="about-bundled">
                    ${bundled.map(
                      (piece) => html`
                        <li key=${piece.name}>
                          <span>${piece.name}</span>
                          ${piece.licence && html`<${Badge} outline>${piece.licence}<//>`}
                        </li>
                      `,
                    )}
                  </ul>
                `
              : html`<p class="about-aside">${vendor.error ? 'The list is read from vendor/LICENSES.txt, which did not load. Open “Third-party licence texts” below to try again.' : 'vendor/LICENSES.txt names no bundled software.'}</p>`}
          <p class="about-aside">The gateway’s Rust dependencies are listed in <span class="mono">Cargo.lock</span> in the repository; each is under its own licence.</p>
        </div>

        <div class="about-details-group">
          <${Disclosure} title="MIT licence text" hint="Switchyard">
            <${CodeBlock} language="text" title="LICENSE" value=${MIT_LICENCE} maxHeight="320px" />
          <//>
          <${Disclosure} title=${vendorFile.title} hint=${vendorFile.path}>
            <${LicenceText} resource=${vendor} path=${vendorFile.path} />
          <//>
          ${fontFiles.map((file) => html`<${LazyLicence} key=${file.id} file=${file} />`)}
        </div>
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export default function About({ route }) {
  const liveOpen = useStore(liveState, (s) => s.status === 'open');

  // With the live connection up, config.reloaded says when to look again and
  // a slow poll catches the rest (a credential that starts or stops resting
  // changes what /models says); without it, poll both.
  const pollMs = liveOpen ? 60_000 : 15_000;
  const status = useResource('/status', { pollMs });
  const models = useResource('/models', { pollMs });

  const reload = () => {
    status.refresh();
    models.refresh();
  };
  // Applied or refused, the frame says the configuration (or the file) moved.
  useLive('config.reloaded', reload);
  // Frames sent while the connection was down, or dropped for a connection
  // that fell behind, are gone: look again.
  useLiveGap(reload);

  // A link to a section (#/about?section=connect) lands on it: once when the
  // page appears, and once more each time the first answer of a request
  // fills in the panels above and moves everything down, unless the reader
  // has moved since. "Answered" turns true once and stays: a retry or a poll
  // after a failure must not pull the page back to the section.
  const readerMoved = useRef(false);
  const landedAt = useRef(null); // where the page's own jump left the window
  useEffect(() => {
    const moved = () => {
      readerMoved.current = true;
    };
    // A scrollbar drag fires none of the input events, only "scroll"; the
    // page's own jump fires that too, and is told apart by where it ended.
    const scrolled = () => {
      if (landedAt.current == null || Math.abs(window.scrollY - landedAt.current) > 2) moved();
    };
    const events = ['wheel', 'touchstart', 'pointerdown', 'keydown'];
    for (const name of events) window.addEventListener(name, moved, { passive: true });
    window.addEventListener('scroll', scrolled, { passive: true });
    return () => {
      for (const name of events) window.removeEventListener(name, moved);
      window.removeEventListener('scroll', scrolled);
    };
  }, []);
  const statusAnswered = status.data !== undefined || status.error != null;
  const modelsAnswered = models.data !== undefined || models.error != null;
  useEffect(() => {
    const target = routeStore.get().query.section;
    if (!target || readerMoved.current) return;
    scrollToSection(target, true);
    landedAt.current = window.scrollY;
    focusSection(target);
  }, [statusAnswered, modelsAnswered]);

  const diagnostics = useRef('');
  useCommands(
    () => [
      {
        id: 'about:copy-diagnostics',
        label: 'Copy diagnostics',
        group: 'About',
        icon: 'copy',
        keywords: 'help support bug report version status',
        run: async () => {
          if (await copyText(diagnostics.current)) toast.success('Diagnostics copied');
          else toast.error('Could not copy the diagnostics', { description: 'The browser refused access to the clipboard. Select the text in the Diagnostics block and copy it by hand.' });
        },
      },
      ...CLIENTS.map((client) => ({
        id: `about:connect:${client.id}`,
        label: `Connect a client: ${client.label}`,
        group: 'About',
        icon: 'plug',
        keywords: 'base url environment variable setup sdk snippet curl example',
        run: () => jumpTo('connect', { client: client.id === CLIENTS[0].id ? null : client.id }),
      })),
    ],
    [],
  );

  const data = status.data;
  const restart = data?.restart_required ?? [];
  const base = useMemo(() => addressChoices(null, document.baseURI)[0].base, []);
  // The notice about a failed refresh goes away, with its button, when the
  // next one works: the keyboard goes to the panel the answer is shown in.
  const stale = !!status.error && !!data;
  const retryStatus = useRetryFocus(stale, status.refresh, () => sectionElement('gateway'));

  return html`
    <${Page} title="About" description="What is running, how to point a client at it, and what the gateway accepts." class="about">
      <${SectionNav} query=${route.query} />

      ${stale &&
      html`
        <${Notice} tone="caution" title="Could not refresh the gateway status" action=${html`<${Button} size="sm" icon="refresh" loading=${status.refreshing} onClick=${retryStatus}>Try again<//>`}>
          ${sentence(status.error.message) || 'The gateway did not give a reason.'} Showing what was loaded at ${formatTime(status.updatedAt)}.
        <//>
      `}
      ${restart.length > 0 &&
      html`
        <${Notice} tone="caution" title="Restart needed" action=${html`<${Button} size="sm" href=${href('/settings')}>Open settings<//>`}>
          ${plural(restart.length, 'setting')} changed since the gateway started and ${restart.length === 1 ? 'takes' : 'take'} effect after a restart: <span class="mono">${restart.join(', ')}</span>.
        <//>
      `}

      <div class="about-top">
        <${GatewayPanel} status=${status} liveOpen=${liveOpen} />
        <${DiagnosticsPanel} status=${status} textRef=${diagnostics} />
      </div>

      <${ConnectPanel} status=${status} models=${models} />
      <${EndpointsPanel} base=${base} />
      <${ReasoningPanel} />

      <div class="about-bottom">
        <${ShortcutsPanel} />
        <${LicencePanel} />
      </div>
    <//>
  `;
}
