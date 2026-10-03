// Models (#/models): what clients can ask for, and where each name goes.
//
//   Models tab    the client-facing model table (GET /models) with routes and
//                 credential availability; a row opens the detail drawer
//   Aliases tab   the editor for virtual models (GET/PUT /aliases)
//   Catalog tab   the metadata built into the gateway (GET /catalog)
//
// URL state: ?tab= models | aliases | catalog, ?open=<model name> (drawer),
// ?q= ?provider= ?avail= ?sort= ?page= (model table), ?cq= ?family= ?csort=
// (catalog).
//
// The page is split into pages/models/*.js: logic.js (pure helpers),
// table.js, detail.js, aliases.js, combobox.js, catalog.js.

import { html, useEffect, useMemo, useRef } from '../../vendor/preact-htm.js';
import { Button, Notice, Page, Tabs } from '../components/index.js';
import { useCommands } from '../lib/commands.js';
import { loadStyles } from '../lib/dom.js';
import { formatTime } from '../lib/format.js';
import { useResource } from '../lib/hooks.js';
import { liveState, useLive, useLiveGap } from '../lib/live.js';
import { navigate, routeStore, setQuery, useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import AliasesTab, { useAliasDraft } from './models/aliases.js';
import CatalogTab from './models/catalog.js';
import ModelDrawer from './models/detail.js';
import { buildRows, nextCooldownEnd } from './models/logic.js';
import ModelsTab from './models/table.js';

await loadStyles('pages/models.css');

const TABS = ['models', 'aliases', 'catalog'];
/** Marks the history entry added by opening the drawer from this page. */
const PUSHED = 'modelsDrawerPushed';
/** Models offered in the command palette; beyond this the search box is the tool. */
const PALETTE_LIMIT = 200;

export default function Models() {
  // Picking a tab is a step in the history: Back returns to the one before.
  const [tabParam, setTab] = useQueryParam('tab', 'models', { push: true });
  const tab = TABS.includes(tabParam) ? tabParam : 'models';
  const [open] = useQueryParam('open', '');

  // Live events say when credentials or the configuration change. Cooldowns
  // also end on their own, without an event, so the table is polled as well:
  // slowly while the live connection is up, faster while it is down.
  const liveStatus = useStore(liveState, (s) => s.status);
  const pollMs = liveStatus === 'open' ? 30_000 : 10_000;

  const models = useResource('/models', { pollMs });
  const providers = useResource('/providers', { pollMs });
  // The alias list is saved as a whole, so a stale copy here would overwrite
  // what someone else saved: it is polled like the rest, live or not.
  const aliases = useResource('/aliases', { pollMs });
  const status = useResource('/status', { pollMs });
  const catalog = useResource(tab === 'catalog' ? '/catalog' : null);

  const refreshRouting = () => {
    models.refresh();
    providers.refresh();
  };
  const refreshAll = () => {
    refreshRouting();
    aliases.refresh();
    status.refresh();
    if (tab === 'catalog') catalog.refresh();
  };

  useLive('credential', refreshRouting);
  useLive('config.reloaded', refreshAll);

  // Frames sent while the connection was down, or dropped for a connection
  // that fell behind, are gone: catch up.
  useLiveGap(refreshAll);

  // A model list that is still being fetched ends without an event: while a
  // provider's discovery is pending the providers and the table are read
  // again every few seconds, so its models appear when the answer does.
  const discovering = (providers.data ?? []).some((p) => p.enabled !== false && p.discovery?.state === 'pending');
  useEffect(() => {
    if (!discovering) return undefined;
    const timer = setTimeout(refreshRouting, 3000);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [discovering, providers.data]);

  // Refetch just after the next cooldown ends, so "back in 3s" turns green
  // on time instead of at the next poll.
  const cooldownEnd = useMemo(() => nextCooldownEnd(providers.data, Date.now()), [providers.data]);
  useEffect(() => {
    if (!cooldownEnd) return undefined;
    const timer = setTimeout(refreshRouting, Math.max(1000, cooldownEnd - Date.now() + 400));
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cooldownEnd]);

  const rows = useMemo(() => (models.data ? buildRows(models.data, providers.data) : undefined), [models.data, providers.data]);
  const rowsByName = useMemo(() => new Map((rows ?? []).map((row) => [row.name, row])), [rows]);

  // Names the providers themselves serve: what an alias of the same name would hide.
  const realNames = useMemo(() => {
    const names = new Set();
    for (const provider of providers.data ?? []) {
      if (provider.enabled === false) continue;
      for (const name of provider.models ?? []) names.add(String(name).toLowerCase());
      // Models written into the provider's settings stay there when an alias
      // takes their name; under a prefix they are served with and without it.
      for (const model of provider.config?.models ?? []) {
        const name = String(model?.alias || model?.id || '').trim().toLowerCase();
        if (!name) continue;
        names.add(name);
        if (provider.prefix) names.add(`${String(provider.prefix).replace(/\/+$/, '').toLowerCase()}/${name}`);
      }
    }
    for (const row of rows ?? []) {
      if (!row.isAlias) names.add(row.name.toLowerCase());
      // The API identifies models hidden by aliases, including discovered
      // models no longer listed under their own name.
      else if (row.shadows_model) names.add(row.name.toLowerCase());
    }
    return names;
  }, [providers.data, rows]);

  // Which upstream model ids are routed to, and by which providers (for the catalog).
  const served = useMemo(() => {
    const map = new Map();
    for (const row of rows ?? []) {
      for (const route of row.routes) {
        const list = map.get(route.upstream_model) ?? [];
        if (!list.includes(route.provider)) list.push(route.provider);
        map.set(route.upstream_model, list);
      }
    }
    return map;
  }, [rows]);

  const draft = useAliasDraft(aliases.data);

  // Opening from the page adds a history entry, so Back closes the drawer.
  // That entry is marked: closing a marked entry steps back to the one it was
  // opened from instead of leaving a second copy of the page in the history.
  // A drawer reached by a link has nothing behind it: there the address is
  // rewritten. Moving from one model to another inside the drawer reuses the
  // entry, so closing always ends on the page the drawer was opened from.
  const closing = useRef(false);
  const setOpen = (name) => {
    if (!name || routeStore.get().query.open === name) return;
    if (routeStore.get().query.open) {
      setQuery({ open: name });
      return;
    }
    setQuery({ open: name }, { replace: false });
    const state = history.state && typeof history.state === 'object' ? history.state : {};
    history.replaceState({ ...state, [PUSHED]: true }, '');
  };
  const closeDrawer = () => {
    if (history.state?.[PUSHED]) {
      // Going back is not instant: a second Escape or click must not go back twice.
      if (closing.current) return;
      closing.current = true;
      history.back();
      setTimeout(() => {
        closing.current = false;
      }, 1000);
    } else {
      setQuery({ open: null });
    }
  };
  useEffect(() => {
    closing.current = false;
  }, [open]);
  const editAliases = () => setQuery({ tab: 'aliases', open: null }, { replace: false });

  useCommands(
    () => [
      { id: 'models:aliases', label: 'Edit aliases', group: 'Models', icon: 'edit', keywords: 'virtual model alias targets', run: () => navigate('/models', { query: { tab: 'aliases' } }) },
      { id: 'models:catalog', label: 'Model catalog', group: 'Models', icon: 'models', keywords: 'built-in metadata context window', run: () => navigate('/models', { query: { tab: 'catalog' } }) },
      ...(rows ?? []).slice(0, PALETTE_LIMIT).map((row) => ({
        id: `model:${row.name}`,
        label: row.name,
        group: 'Models',
        icon: 'models',
        hint: row.isAlias ? 'Alias' : row.availability.label,
        keywords: [row.info.display_name, ...row.providerNames].filter(Boolean).join(' '),
        run: () => navigate('/models', { query: { open: row.name } }),
      })),
    ],
    [rows],
  );

  // A refetch that failed with data on screen: keep the data, say so.
  const staleError = (models.data && models.error) || (models.data && providers.data && providers.error) || null;
  const refreshing = models.refreshing || providers.refreshing || aliases.refreshing || status.refreshing || catalog.refreshing;

  return html`
    <${Page}
      title="Models"
      description="The model names clients can ask for and where each one is routed."
      class="models"
      actions=${html`<${Button} icon="refresh" loading=${refreshing} onClick=${refreshAll}>Refresh<//>`}
    >
      <${Tabs}
        label="Models views"
        value=${tab}
        onChange=${setTab}
        tabs=${[
          { id: 'models', label: 'Models', count: rows ? rows.length : undefined },
          { id: 'aliases', label: draft.dirty ? 'Aliases, unsaved' : 'Aliases', count: draft.rows ? draft.rows.length : aliases.data?.length },
          { id: 'catalog', label: 'Catalog', count: catalog.data ? catalog.data.length : undefined },
        ]}
      />

      ${tab !== 'catalog' &&
      staleError &&
      html`
        <${Notice} tone="caution" title="Could not refresh the model table" action=${html`<${Button} size="sm" icon="refresh" onClick=${refreshRouting}>Try again<//>`}>
          ${staleError.message} Showing what was loaded at ${formatTime(models.updatedAt)}.
        <//>
      `}
      ${tab !== 'catalog' &&
      models.data &&
      !providers.data &&
      providers.error &&
      html`
        <${Notice} tone="caution" title="Provider details could not be loaded" action=${html`<${Button} size="sm" icon="refresh" onClick=${providers.refresh}>Try again<//>`}>
          ${providers.error.message} Routes that cannot serve right now are shown as not available, without the reason.
        <//>
      `}
      ${tab === 'aliases' &&
      !models.data &&
      models.error &&
      !models.loading &&
      html`
        <${Notice} tone="caution" title="Could not load the model table" action=${html`<${Button} size="sm" icon="refresh" onClick=${refreshRouting}>Try again<//>`}>
          ${models.error.message} The aliases can be edited, but which of them route, and whether a target matches a model, is not shown until it loads.
        <//>
      `}

      ${tab === 'models' &&
      html`
        <${ModelsTab}
          rows=${rows}
          providers=${providers.data}
          providersLoading=${providers.loading}
          loading=${models.loading}
          error=${models.error}
          onRetry=${refreshRouting}
          open=${open}
          onOpen=${setOpen}
        />
      `}
      ${tab === 'aliases' &&
      html`
        <${AliasesTab}
          saved=${aliases.data}
          loading=${aliases.loading}
          error=${aliases.error}
          onRetry=${aliases.refresh}
          draft=${draft}
          modelRows=${rows}
          modelsState=${rows ? 'ready' : models.loading ? 'loading' : 'failed'}
          rowsByName=${rowsByName}
          realNames=${realNames}
          onFresh=${aliases.mutate}
          onSaved=${(list) => {
            aliases.mutate(list);
            refreshRouting();
            status.refresh();
          }}
          onOpen=${setOpen}
        />
      `}
      ${tab === 'catalog' && html`<${CatalogTab} catalog=${catalog.data} loading=${catalog.loading} error=${catalog.error} onRetry=${catalog.refresh} served=${served} />`}

      <${ModelDrawer}
        name=${open}
        row=${open ? rowsByName.get(open) : undefined}
        rowsByName=${rowsByName}
        providers=${providers.data}
        aliases=${aliases.data}
        status=${status.data}
        loading=${models.loading}
        error=${models.data ? null : models.error}
        onRetry=${refreshRouting}
        onClose=${closeDrawer}
        onOpen=${setOpen}
        onEditAliases=${editAliases}
      />
    <//>
  `;
}
