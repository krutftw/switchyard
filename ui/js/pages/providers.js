// Providers (#/providers): the upstreams the gateway routes to and the state
// of their credentials. The list, its filters and the live updates live
// here; the detail drawer, the editor and the page's logic are in
// ./providers/.
//
// URL state: q (text filter), kind, sort ("health:asc"), open (provider in
// the detail drawer), tab (its tab), edit (provider in the editor), new
// (quick-start id, or "1", for the editor in create mode).

import { html, useEffect, useMemo, useRef, useState } from '../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  EmptyState,
  Input,
  Menu,
  Notice,
  Page,
  Panel,
  Select,
  StatusLamp,
  Switch,
  Table,
  confirm,
  sortRows,
  toast,
} from '../components/index.js';
import { api } from '../lib/api.js';
import { useCommands } from '../lib/commands.js';
import { loadStyles } from '../lib/dom.js';
import { DASH, formatCompact, formatDuration, formatNumber, formatPercent, plural } from '../lib/format.js';
import { useResource } from '../lib/hooks.js';
import { liveState, useLive } from '../lib/live.js';
import { href, parseHash, routeStore, setQuery, useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import ProviderDetail from './providers/detail.js';
import ProviderEditor from './providers/editor.js';
import { KINDS, QUICK_STARTS, applyCredentialFrame, hasCountdown, kindInfo, providerHealth, replaceProvider } from './providers/model.js';
import { CredentialLamps, LampLegend, useServerNow } from './providers/parts.js';

await loadStyles('pages/providers.css');

const providerPath = (name) => `/providers/${encodeURIComponent(name)}`;

// The orders the list is offered in. One control, on every width: it also
// shows an order picked by clicking a column header, and it is the way back
// to the order of the configuration file.
const ORDERS = [
  { value: '', label: 'Configured order' },
  { value: 'health:asc', label: 'Needs attention first' },
  { value: 'name:asc', label: 'Name, A to Z' },
  { value: 'name:desc', label: 'Name, Z to A' },
  { value: 'requests:desc', label: 'Most requests first' },
  { value: 'latency:desc', label: 'Slowest first' },
  { value: 'models:desc', label: 'Most models first' },
  { value: 'priority:desc', label: 'Highest priority first' },
];

/** The part of a route that says which form is open: "edit=name", "new=id" or "". */
const editorParamOf = (query) => (query.edit ? `edit=${query.edit}` : query.new ? `new=${query.new}` : '');

function parseSort(text) {
  const [key, dir] = String(text ?? '').split(':');
  return key ? { key, dir: dir === 'desc' ? 'desc' : 'asc' } : null;
}

function matchesText(provider, needle) {
  if (!needle) return true;
  const hay = [
    provider.name,
    provider.kind,
    kindInfo(provider.kind).label,
    provider.effective_base_url,
    provider.prefix,
    ...(provider.models ?? []),
    ...(provider.credentials ?? []).map((c) => c.label),
  ];
  return hay.some((text) => typeof text === 'string' && text.toLowerCase().includes(needle));
}

/** What a provider is, and the shortest ways to add one. */
function FirstProvider({ onAdd }) {
  const picks = ['openai', 'anthropic', 'gemini', 'openrouter', 'ollama', 'mock'].map((id) => QUICK_STARTS.find((q) => q.id === id)).filter(Boolean);
  return html`
    <${EmptyState}
      icon="providers"
      title="No providers yet"
      description="A provider is an upstream the gateway sends requests to: an API such as OpenAI or Anthropic, or any OpenAI-compatible server, with the keys to call it. Add one and its models become available to your clients."
      action=${html`
        <div class="prov-first">
          <${Button} variant="primary" icon="plus" onClick=${() => onAdd('1')}>Add provider<//>
          <div class="prov-quick">
            <span class="plate-label">Or start from</span>
            <div class="prov-quick-list">
              ${picks.map((quick) => html`<${Button} key=${quick.id} size="sm" onClick=${() => onAdd(quick.id)}>${quick.label}<//>`)}
            </div>
          </div>
        </div>
      `}
    />
  `;
}

export default function Providers() {
  const liveStatus = useStore(liveState, (s) => s.status);
  const isLive = liveStatus === 'open';
  // Live frames carry failures and cooldowns; counters of successful traffic
  // come from refetching. Without the live connection, poll faster.
  const providers = useResource('/providers', { pollMs: isLive ? 20_000 : 5_000 });
  const list = Array.isArray(providers.data) ? providers.data : null;
  const listRef = useRef(list);
  listRef.current = list;

  const [q, setQ] = useQueryParam('q', '');
  const [kind, setKind] = useQueryParam('kind', '');
  const [sortParam, setSortParam] = useQueryParam('sort', '');
  const [openName, setOpenName] = useQueryParam('open', '');
  const [tab, setTab] = useQueryParam('tab', 'credentials');
  const [editName] = useQueryParam('edit', '');
  const [newParam] = useQueryParam('new', '');

  // Every second while something counts down or a drawer shows "12s ago"; otherwise rarely.
  const now = useServerNow(hasCountdown(list) || openName ? 1000 : 30_000);

  // ---- Live ---------------------------------------------------------------

  const refreshTimer = useRef(null);
  const refreshSoon = () => {
    if (refreshTimer.current) return;
    refreshTimer.current = setTimeout(() => {
      refreshTimer.current = null;
      providers.refresh();
    }, 2000);
  };
  useEffect(() => () => clearTimeout(refreshTimer.current), []);

  useLive('credential', (frame) => {
    const current = listRef.current;
    // A credential the list does not have yet: the configuration moved on.
    if (!current || applyCredentialFrame(current, frame) === current) {
      providers.refresh();
      return;
    }
    providers.mutate((data) => applyCredentialFrame(data, frame));
  });
  useLive('config.reloaded', () => providers.refresh());
  useLive('request.finished', refreshSoon);

  // Frames sent while the connection was down are gone: refetch when it returns.
  const wasLive = useRef(isLive);
  useEffect(() => {
    if (isLive && !wasLive.current) providers.refresh();
    wasLive.current = isLive;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isLive]);

  // ---- Rows ---------------------------------------------------------------

  const rows = useMemo(() => (list ?? []).map((provider) => ({ name: provider.name, provider, health: providerHealth(provider, now) })), [list, now]);

  // A cooldown that ran out on this clock has ended on the gateway too: ask.
  const resting = rows.reduce((sum, row) => sum + row.health.counts.cooling + row.health.states.reduce((n, s) => n + s.resting.length, 0), 0);
  const restingBefore = useRef(resting);
  useEffect(() => {
    if (resting < restingBefore.current) refreshSoon();
    restingBefore.current = resting;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [resting]);

  const totals = useMemo(() => {
    const sum = { ready: 0, cooling: 0, disabled: 0, unusable: 0, idle: 0, unknown: 0 };
    for (const row of rows) for (const key of Object.keys(sum)) sum[key] += row.health.counts[key];
    return sum;
  }, [rows]);

  const kindsPresent = useMemo(() => KINDS.filter((k) => (list ?? []).some((p) => p.kind === k.value)), [list]);
  const needle = q.trim().toLowerCase();
  const filtered = rows.filter((row) => (!kind || row.provider.kind === kind) && matchesText(row.provider, needle));
  const filtering = !!needle || !!kind;

  // ---- Mutations ----------------------------------------------------------

  const [toggling, setToggling] = useState(() => new Set());

  const toggleEnabled = async (provider, enabled) => {
    const { name } = provider;
    setToggling((set) => new Set(set).add(name));
    // Optimistic: the switch moves at once and moves back if the gateway refuses.
    providers.mutate((data) => (Array.isArray(data) ? data.map((p) => (p.name === name ? { ...p, enabled } : p)) : data));
    try {
      // The freshest entry, so an edit made elsewhere a moment ago is not undone.
      const fresh = await api.get(providerPath(name));
      const view = await api.put(providerPath(name), { ...fresh.config, enabled });
      providers.mutate((data) => replaceProvider(data, view));
      toast.success(enabled ? `Provider ${name} enabled` : `Provider ${name} disabled`, {
        description: enabled ? 'The router uses it again.' : 'The router skips it until it is enabled.',
      });
    } catch (error) {
      providers.mutate((data) => (Array.isArray(data) ? data.map((p) => (p.name === name ? { ...p, enabled: !enabled } : p)) : data));
      if (!error?.aborted) toast.error(`Could not ${enabled ? 'enable' : 'disable'} ${name}`, { description: error?.message });
    } finally {
      setToggling((set) => {
        const next = new Set(set);
        next.delete(name);
        return next;
      });
      providers.refresh();
    }
  };

  const removeProvider = async (provider) => {
    const credentials = (provider.credentials ?? []).length;
    const ok = await confirm({
      danger: true,
      title: `Delete provider ${provider.name}?`,
      message: `${plural(credentials, 'credential')} and ${plural(provider.model_count ?? 0, 'model')} go with it. Requests for models that only ${provider.name} serves will be rejected, and its entry is removed from the configuration file.`,
      confirmLabel: 'Delete provider',
      action: () => api.del(providerPath(provider.name)),
    });
    if (!ok) return;
    toast.success(`Provider ${provider.name} deleted`);
    providers.mutate((data) => (Array.isArray(data) ? data.filter((p) => p.name !== provider.name) : data));
    if (openName === provider.name) setQuery({ open: null, tab: null });
    providers.refresh();
  };

  // ---- Editor -------------------------------------------------------------

  const [nonce, setNonce] = useState(0);
  const dirtyRef = useRef(false);
  const savingRef = useRef(false);
  const closingRef = useRef(false);
  const takenNames = useMemo(() => (list ?? []).map((p) => p.name), [list]);

  // The form of `edit=<name>` can be carried over to the provider that name
  // was changed to (elsewhere, or by the first half of a two-part save)
  // without starting afresh: { from: the name in the URL, to: the provider }.
  const [retarget, setRetarget] = useState(null);
  const editing = retarget && retarget.from === editName ? retarget.to : editName;

  // The provider being edited as last seen, and the names of the list before
  // this render's: what is needed to keep a form whose provider vanished.
  const lastEdited = useRef(null);
  const knownNames = useRef(null);
  const goneInfo = useRef(null);

  let editorTarget = null;
  if (editName) {
    const provider = (list ?? []).find((p) => p.name === editing);
    const key = `edit:${editName}:${nonce}`;
    if (provider) {
      lastEdited.current = { key, provider };
      goneInfo.current = null;
      editorTarget = { key, mode: 'edit', provider };
    } else if (list && lastEdited.current?.key === key && savingRef.current && goneInfo.current?.key !== key) {
      // Its own save is on the way and has renamed it (a save can take two
      // requests): the form stays as it is until the save has answered.
      editorTarget = { key, mode: 'edit', provider: lastEdited.current.provider };
    } else if (list && lastEdited.current?.key === key && (dirtyRef.current || goneInfo.current?.key === key)) {
      // The provider left the configuration (renamed or deleted elsewhere)
      // while its form holds unsaved changes: the form stays and says so.
      if (goneInfo.current?.key !== key) {
        const was = lastEdited.current.provider;
        const appeared = list.filter((p) => !(knownNames.current ?? []).includes(p.name));
        goneInfo.current = { key, candidate: appeared.length === 1 && appeared[0].kind === was.kind ? appeared[0].name : null };
      }
      editorTarget = { key, mode: 'edit', provider: lastEdited.current.provider, gone: { candidate: goneInfo.current.candidate } };
    }
  } else if (newParam) {
    editorTarget = { key: `new:${newParam}:${nonce}`, mode: 'new', quick: QUICK_STARTS.find((x) => x.id === newParam) ?? null };
  }
  useEffect(() => {
    knownNames.current = takenNames;
  }, [takenNames]);

  const openEditor = (provider) => setQuery({ edit: provider.name, new: null }, { replace: false });
  const openNew = (id = '1') => setQuery({ new: id, edit: null }, { replace: false });
  const closeEditor = (extra = {}) => {
    closingRef.current = true;
    dirtyRef.current = false;
    lastEdited.current = null;
    goneInfo.current = null;
    setRetarget(null);
    setQuery({ edit: null, new: null, ...extra });
    setNonce((n) => n + 1);
  };

  // Leaving a form with unsaved changes asks first, whatever the way out:
  // the Back button, a link, the command palette, another form. The address
  // has changed by the time anyone can object, so this puts the form's
  // address back before the router looks (it reads the address on
  // "hashchange"; "popstate" comes first, for links and for Back alike) and
  // then asks. Filters and the detail drawer change the address too and
  // pass: the form stays open under them.
  const leaveAsked = useRef(false);
  useEffect(() => {
    const onAddressChange = (event) => {
      if (!dirtyRef.current) return;
      const from = routeStore.get();
      const to = parseHash(location.hash);
      if (to.path === from.path && editorParamOf(to.query) === editorParamOf(from.query)) return;
      // Where "hashchange" is the first to tell, keep it from the router.
      if (event.type === 'hashchange') event.stopImmediatePropagation();
      const wanted = location.hash;
      const url = new URL(location.href);
      url.hash = href(from.path, from.query);
      history.replaceState(history.state, '', url);
      if (leaveAsked.current) return;
      leaveAsked.current = true;
      confirm({
        danger: true,
        title: 'Discard unsaved changes?',
        message: 'What you entered in the provider form has not been saved.',
        confirmLabel: 'Discard changes',
        cancelLabel: 'Keep editing',
      }).then((ok) => {
        leaveAsked.current = false;
        if (!ok) return;
        dirtyRef.current = false;
        lastEdited.current = null;
        goneInfo.current = null;
        setRetarget(null);
        setNonce((n) => n + 1);
        location.hash = wanted;
      });
    };
    window.addEventListener('popstate', onAddressChange);
    window.addEventListener('hashchange', onAddressChange, true);
    return () => {
      window.removeEventListener('popstate', onAddressChange);
      window.removeEventListener('hashchange', onAddressChange, true);
    };
  }, []);

  // The editor's place in the URL went away without closeEditor(). The
  // listener above stops that while there are unsaved changes; this is what
  // is left: start the next form fresh, and, should a browser have let the
  // change through to the router first, bring the form back and ask.
  const editorParam = editorParamOf({ edit: editName, new: newParam });
  const paramBefore = useRef(editorParam);
  useEffect(() => {
    const before = paramBefore.current;
    paramBefore.current = editorParam;
    if (!before || editorParam) return;
    if (closingRef.current) {
      closingRef.current = false;
      return;
    }
    if (!dirtyRef.current) {
      setNonce((n) => n + 1);
      return;
    }
    const at = before.indexOf('=');
    setQuery({ [before.slice(0, at)]: before.slice(at + 1) }, { replace: false });
    confirm({
      danger: true,
      title: 'Discard unsaved changes?',
      message: 'What you entered in the provider form has not been saved.',
      confirmLabel: 'Discard changes',
      cancelLabel: 'Keep editing',
    }).then((ok) => {
      if (ok) closeEditor();
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editorParam]);

  // A link to edit a provider that is not there (renamed, deleted), or a
  // provider that vanished under a form with nothing to lose.
  useEffect(() => {
    if (!editName || !list || editorTarget) return;
    toast.info(`No provider named ${editName}`, { description: 'It may have been renamed or deleted.' });
    closeEditor();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editName, list, editorTarget == null]);

  const onSaved = (view, previousName) => {
    providers.mutate((data) => replaceProvider(Array.isArray(data) ? data : [], view, previousName ?? view.name));
    // A new provider opens in the detail drawer, where it can be tested; an
    // edited one stays where it was (under its new name, if it was renamed).
    if (previousName == null) closeEditor({ open: view.name, tab: null });
    else if (openName === previousName && view.name !== previousName) closeEditor({ open: view.name });
    else closeEditor();
    providers.refresh();
  };

  // The first of two requests of a save went through and the second did not:
  // the list shows what is stored, and the form stays with that entry.
  const onPartial = (view, previousName) => {
    providers.mutate((data) => replaceProvider(Array.isArray(data) ? data : [], view, previousName));
    if (view.name !== editName) setRetarget({ from: editName, to: view.name });
    providers.refresh();
  };

  // ---- Palette ------------------------------------------------------------

  useCommands(
    () => [
      { id: 'providers:add', label: 'Add provider', group: 'Providers', icon: 'plus', run: () => openNew('1') },
      ...(list ?? []).slice(0, 200).map((p) => ({
        id: `provider:${p.name}`,
        label: p.name,
        group: 'Providers',
        icon: 'providers',
        hint: p.kind,
        keywords: `${p.effective_base_url} ${p.prefix}`,
        run: () => setQuery({ open: p.name }, { replace: false }),
      })),
    ],
    [takenNames.join('\n')],
  );

  // ---- Table --------------------------------------------------------------

  const columns = [
    {
      key: 'name',
      header: 'Provider',
      primary: true,
      sortable: true,
      render: ({ provider }) => html`
        <div class="prov-id">
          <span class="prov-name mono" title=${provider.name}>${provider.name}</span>
          <div class="prov-id-sub">
            <${Badge} mono>${provider.kind}<//>
            ${provider.prefix && html`<${Badge} mono outline title=${`Models are also served as ${provider.prefix}/model`}>${provider.prefix}/<//>`}
            <span class="prov-url mono" title=${provider.effective_base_url}>${provider.effective_base_url || 'No base URL'}</span>
          </div>
        </div>
      `,
    },
    {
      key: 'health',
      header: 'State',
      sortable: true,
      sortValue: (row) => row.health.rank,
      render: ({ health }) => html`
        <div class="prov-state">
          <${StatusLamp} tone=${health.tone} label=${health.label} />
          ${health.detail && html`<span class="prov-state-detail">${health.detail}</span>`}
        </div>
      `,
    },
    {
      key: 'credentials',
      header: 'Credentials',
      render: ({ health }) => html`<${CredentialLamps} states=${health.states} counts=${health.counts} total=${health.total} />`,
    },
    {
      key: 'models',
      header: 'Models',
      align: 'right',
      num: true,
      sortable: true,
      sortValue: (row) => row.provider.model_count ?? 0,
      render: ({ provider }) => formatNumber(provider.model_count ?? 0),
    },
    {
      key: 'priority',
      header: 'Priority',
      align: 'right',
      num: true,
      sortable: true,
      hideOnPhone: true,
      sortValue: (row) => row.provider.priority ?? 0,
      render: ({ provider }) => formatNumber(provider.priority ?? 0),
    },
    {
      key: 'requests',
      header: 'Requests',
      align: 'right',
      num: true,
      sortable: true,
      sortValue: (row) => row.health.requests,
      render: ({ health }) =>
        health.requests > 0
          ? html`
              <span class="prov-traffic">
                <span>${formatCompact(health.requests)}</span>
                <span class="prov-traffic-fail" data-tone=${health.failureRatio >= 0.5 ? 'stop' : health.failureRatio >= 0.1 ? 'caution' : undefined}>
                  ${health.failures > 0 ? `${formatPercent(health.failureRatio)} failed` : 'none failed'}
                </span>
              </span>
            `
          : DASH,
    },
    {
      key: 'latency',
      header: 'Latency',
      align: 'right',
      num: true,
      sortable: true,
      hideOnPhone: true,
      sortValue: (row) => row.health.latency,
      render: ({ health }) => formatDuration(health.latency),
    },
    {
      key: 'enabled',
      header: 'Enabled',
      render: ({ provider }) => html`
        <${Switch}
          class="prov-switch"
          aria-label=${`${provider.name} enabled`}
          checked=${provider.enabled}
          disabled=${toggling.has(provider.name)}
          onChange=${(next) => toggleEnabled(provider, next)}
        />
      `,
    },
    {
      key: 'actions',
      header: '',
      align: 'right',
      render: ({ provider }) => html`
        <${Menu}
          label=${`Actions for ${provider.name}`}
          items=${[
            { label: 'Open details', icon: 'sidebar', onSelect: () => setQuery({ open: provider.name, tab: null }, { replace: false }) },
            { label: 'Edit', icon: 'edit', onSelect: () => openEditor(provider) },
            { separator: true },
            { label: 'Delete provider', icon: 'trash', danger: true, onSelect: () => removeProvider(provider) },
          ]}
        />
      `,
    },
  ];

  // An order the URL names that no sortable column stands for is no order.
  const wantedSort = parseSort(sortParam);
  const sortColumn = wantedSort ? columns.find((c) => c.key === wantedSort.key && c.sortable) : null;
  const sort = sortColumn ? wantedSort : null;
  const sorted = sort ? sortRows(filtered, sortColumn, sort.dir) : filtered;
  // The order control also names an order picked on a column header.
  const sortValue = sort ? `${sort.key}:${sort.dir}` : '';
  const orderOptions = ORDERS.some((o) => o.value === sortValue)
    ? ORDERS
    : [...ORDERS, { value: sortValue, label: sortValue === 'health:desc' ? 'Healthy first' : `${sortColumn.header}, ${sort.dir === 'asc' ? 'lowest first' : 'highest first'}` }];
  const opened = openName ? (list ?? []).find((p) => p.name === openName) ?? null : null;
  const empty = list != null && list.length === 0;
  const stale = providers.error && list != null;
  const kindOptions = kindsPresent.map((k) => ({ value: k.value, label: k.label }));
  // A kind named in the URL that no provider has (any more) stays selectable, so it can be cleared.
  if (kind && !kindOptions.some((o) => o.value === kind)) kindOptions.push({ value: kind, label: kindInfo(kind).label });
  const shownCount = filtering ? `${formatNumber(sorted.length)} of ${plural(rows.length, 'provider')}` : plural(rows.length, 'provider');
  const footText = `${shownCount}. ${isLive ? 'Cooldowns and failures arrive live.' : 'The live connection is down: the list refreshes every 5 seconds.'}`;

  return html`
    <${Page}
      title="Providers"
      description="Upstreams the gateway routes to, and the state of their credentials."
      actions=${empty ? null : html`<${Button} variant="primary" icon="plus" onClick=${() => openNew('1')}>Add provider<//>`}
    >
      ${stale &&
      html`
        <${Notice} tone="caution" title="Showing the last state that loaded" action=${html`<${Button} size="sm" icon="refresh" loading=${providers.refreshing} onClick=${providers.refresh}>Try again<//>`}>
          Could not refresh the list: ${providers.error.message}
        <//>
      `}

      <${Panel} flush>
        ${empty
          ? html`<${FirstProvider} onAdd=${openNew} />`
          : html`
              <div class="prov-toolbar">
                <${Input}
                  class="prov-search"
                  size="sm"
                  icon="search"
                  type="search"
                  aria-label="Filter providers"
                  placeholder="Filter by name, URL, prefix, model or credential"
                  value=${q}
                  onChange=${setQ}
                />
                <${Select}
                  class="prov-kind-filter"
                  size="sm"
                  aria-label="Kind"
                  value=${kind}
                  onChange=${setKind}
                  placeholder="All kinds"
                  options=${kindOptions}
                />
                <${Select} class="prov-order-filter" size="sm" aria-label="Order of the list" value=${sortValue} onChange=${setSortParam} options=${orderOptions} />
                ${list != null && html`<${LampLegend} totals=${totals} />`}
              </div>
              <${Table}
                class="prov-table"
                rowKey="name"
                columns=${columns}
                rows=${list != null ? sorted : undefined}
                loading=${providers.loading}
                error=${providers.error}
                onRetry=${providers.refresh}
                sort=${sort}
                onSort=${(next) => setSortParam(next ? `${next.key}:${next.dir}` : '')}
                onRowClick=${(row) => setQuery({ open: row.name, tab: null }, { replace: false })}
                selectedKey=${openName || null}
                caption="Providers"
                empty=${filtering
                  ? {
                      icon: 'search',
                      title: 'No provider matches',
                      description: `${plural(rows.length, 'provider')} configured, none of them ${needle ? `contains "${q.trim()}"` : 'is of this kind'}${needle && kind ? ' in this kind' : ''}.`,
                      action: html`<${Button} size="sm" onClick=${() => setQuery({ q: null, kind: null })}>Clear filters<//>`,
                    }
                  : { icon: 'providers', title: 'No providers' }}
              />
            `}
      <//>

      ${list != null &&
      !empty &&
      html`
        <p class="prov-foot">${footText}</p>
      `}

      <${ProviderDetail}
        name=${openName}
        provider=${opened}
        state=${list != null ? 'ready' : providers.error ? 'error' : 'loading'}
        error=${providers.error}
        now=${now}
        tab=${tab}
        onTab=${setTab}
        onClose=${() => setQuery({ open: null, tab: null })}
        onEdit=${openEditor}
        onDelete=${removeProvider}
        onPatch=${(view) => providers.mutate((data) => replaceProvider(data, view))}
        onRefresh=${providers.refresh}
        onRetry=${providers.refresh}
        onToggle=${toggleEnabled}
        toggling=${opened ? toggling.has(opened.name) : false}
      />

      <${ProviderEditor}
        target=${editorTarget}
        takenNames=${takenNames}
        onClose=${() => closeEditor()}
        onSaved=${onSaved}
        onPartial=${onPartial}
        onRetarget=${(name) => setRetarget({ from: editName, to: name })}
        dirtyRef=${dirtyRef}
        savingRef=${savingRef}
      />
    <//>
  `;
}
