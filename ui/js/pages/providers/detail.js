// Providers page: the detail drawer. One provider's credentials with their
// runtime state, a connection test, and the models it serves.

import { html, useEffect, useMemo, useRef, useState } from '../../../vendor/preact-htm.js';
import {
  Badge,
  Button,
  CopyButton,
  Drawer,
  EmptyState,
  ErrorState,
  Input,
  KeyValue,
  Menu,
  Notice,
  Select,
  Skeleton,
  StatusLamp,
  Switch,
  Table,
  Tabs,
  toast,
  toneForStatus,
} from '../../components/index.js';
import { api } from '../../lib/api.js';
import { DASH, formatDuration, formatNumber, formatPercent, formatRelativeTime, plural, sentence } from '../../lib/format.js';
import { useAsync, useResource } from '../../lib/hooks.js';
import { discoveryInfo, explainCooldown, kindInfo, providerHealth, reasonNoun } from './model.js';
import { Section } from './parts.js';

const credentialPath = (id, action) => `/credentials/${encodeURIComponent(id)}/${action}`;
const providerPath = (name, action = '') => `/providers/${encodeURIComponent(name)}${action}`;

// ---------------------------------------------------------------------------
// Test connection
// ---------------------------------------------------------------------------

function TestResult({ result }) {
  const facts = html`
    <span class="prov-test-facts">
      ${result.model && html`<span>model <span class="mono">${result.model}</span></span>`}
      ${result.credential && html`<span>credential <span class="mono">${result.credential}</span></span>`}
      ${result.latency_ms > 0 && html`<span>${result.ok ? 'answered in' : 'after'} <span class="num">${formatDuration(result.latency_ms)}</span></span>`}
    </span>
  `;
  if (result.ok) {
    return html`
      <${Notice} tone="clear" title=${`The provider answered (HTTP ${result.status})`}>${facts}<//>
    `;
  }
  const title = result.status > 0 ? `The test failed with HTTP ${result.status}` : result.model ? 'No response from the provider' : 'The test could not be sent';
  return html`
    <${Notice} tone="stop" title=${title}>
      <span class="prov-break">${result.error || 'The provider gave no reason.'}</span>
      ${(result.model || result.credential) && facts}
    <//>
  `;
}

function TestPanel({ provider, onTested }) {
  const [model, setModel] = useState('');
  // The gateway waits up to 60 s for the upstream; give it a little longer.
  const test = useAsync(() => api.post(providerPath(provider.name, '/test'), model ? { model } : {}, { timeout: 70_000 }));
  const models = provider.models ?? [];
  const picked = models.includes(model) ? model : '';

  const run = async () => {
    await test.run();
    onTested?.();
  };

  // The mock provider's scripted 401 is the one failure that rests nothing:
  // a rejected key would rest the credential, and the mock models share one.
  const description =
    provider.kind === 'mock'
      ? 'Sends one small request through the first usable credential, whatever its cooldown. A failure rests the model as a real one would, except mock-error-401: its scripted rejection is answered and leaves the credential in rotation.'
      : 'Sends one small request through the first usable credential, whatever its cooldown. A failure rests the model as a real one would.';

  return html`
    <${Section} title="Test connection" description=${description}>
      <div class="prov-test-row">
        <${Select}
          class="prov-test-model"
          aria-label="Model to test with"
          value=${picked}
          onChange=${setModel}
          placeholder=${models.length > 0 ? 'First model of the provider' : 'No models to test with'}
          options=${models}
          disabled=${test.loading || models.length === 0}
        />
        <${Button} icon="zap" loading=${test.loading} onClick=${run}>Test connection<//>
      </div>
      ${test.loading && html`<p class="prov-note" role="status">Waiting for the provider. This can take up to a minute.</p>`}
      ${!test.loading && test.error && html`<${Notice} tone="stop" title="The test did not run">${test.error.message}<//>`}
      ${!test.loading && !test.error && test.data && html`<${TestResult} result=${test.data} />`}
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

function LastError({ error, now }) {
  return html`
    <div class="prov-cred-error">
      <div class="prov-cred-error-head">
        <span class="plate-label">Last error</span>
        <${Badge} mono tone=${toneForStatus(error.status)}>${error.status > 0 ? error.status : 'no response'}<//>
        <span class="prov-cred-error-class">${reasonNoun(error.class)}</span>
        <span class="faint">${formatRelativeTime(error.at, now)}</span>
      </div>
      <p class="prov-cred-error-text prov-break">${error.message || 'The provider gave no reason.'}</p>
      ${error.model && html`<p class="prov-cred-error-model faint">on <span class="mono prov-break">${error.model}</span></p>`}
    </div>
  `;
}

function CredentialItem({ credential, state, now, onPatch, onRefresh }) {
  const reset = useAsync(() => api.post(credentialPath(credential.id, 'reset'), {}));
  const toggle = useAsync((enable) => api.post(credentialPath(credential.id, enable ? 'enable' : 'disable'), {}));
  const name = credential.label || credential.masked_key || credential.id;
  // The credential's own switch, whatever its provider's: that is what Enable and Disable move.
  const off = credential.disabled === true || state.key === 'disabled';
  const providerOff = state.key === 'idle';
  const canReset = state.key === 'cooling' || (credential.model_cooldowns ?? []).length > 0 || credential.consecutive_failures > 0 || credential.cooldown_until != null;

  const doReset = async () => {
    if (await reset.run()) {
      toast.success('Cooldown cleared', { description: `${name} is back in rotation.` });
      onRefresh();
    } else {
      toast.error('Could not reset the cooldown', { description: reset.error?.message });
    }
  };

  const doToggle = async () => {
    const view = await toggle.run(off);
    if (view) {
      // The answer is the provider's new view: ids stay, source and index may move.
      if (typeof view === 'object') onPatch(view);
      toast.success(off ? 'Credential enabled' : 'Credential disabled', {
        description: off
          ? providerOff
            ? `${name} takes requests again once the provider is enabled.`
            : `${name} takes requests again.`
          : `${name} takes no requests. The change is saved in the configuration.`,
      });
      onRefresh();
    } else {
      toast.error(off ? 'Could not enable the credential' : 'Could not disable the credential', { description: toggle.error?.message });
    }
  };

  const statusOf = (rest) => (credential.last_error && credential.last_error.class === rest.reason && credential.last_error.model === rest.model ? credential.last_error.status : 0);
  const showKey = credential.masked_key && credential.masked_key !== credential.label;

  return html`
    <li class="prov-cred" data-state=${state.key}>
      <div class="prov-cred-head">
        <div class="prov-cred-name">
          <span class="prov-cred-label prov-break">${name}</span>
          ${showKey && html`<span class="mono faint prov-break">${credential.masked_key}</span>`}
        </div>
        <div class="prov-cred-actions">
          ${canReset && html`<${Button} size="sm" icon="refresh" loading=${reset.loading} onClick=${doReset}>Reset cooldown<//>`}
          <${Menu}
            label=${`Actions for credential ${name}`}
            size="sm"
            items=${[
              { label: 'Reset cooldown', icon: 'refresh', onSelect: doReset, disabled: reset.loading },
              off
                ? { label: 'Enable credential', icon: 'play', onSelect: doToggle, disabled: toggle.loading }
                : { label: 'Disable credential', icon: 'pause', onSelect: doToggle, disabled: toggle.loading },
            ]}
          />
        </div>
      </div>

      <div class="prov-cred-state">
        <${StatusLamp} tone=${state.tone} label=${state.label} />
        ${state.text && html`<span class="prov-cred-why">${state.text}</span>`}
      </div>

      <dl class="prov-stats">
        <div><dt>Requests</dt><dd class="num">${formatNumber(credential.requests ?? 0)}</dd></div>
        <div><dt>Succeeded</dt><dd class="num">${formatNumber(credential.successes ?? 0)}</dd></div>
        <div>
          <dt>Failed</dt>
          <dd class="num">
            ${formatNumber(credential.failures ?? 0)}
            ${credential.requests > 0 && credential.failures > 0 && html`<span class="faint"> (${formatPercent(credential.failures / credential.requests)})</span>`}
          </dd>
        </div>
        <div><dt>Latency</dt><dd class="num">${formatDuration(credential.latency_ms)}</dd></div>
        <div><dt>Last used</dt><dd>${formatRelativeTime(credential.last_used_at, now)}</dd></div>
      </dl>

      ${state.resting.length > 0 &&
      html`
        <div class="prov-rests">
          <span class="plate-label">Resting models</span>
          <ul class="prov-rest-list">
            ${state.resting.map(
              (rest) => html`
                <li key=${rest.model}>
                  <span class="lamp" data-tone="caution" aria-hidden="true"></span>
                  <span class="mono prov-break">${rest.model}</span>
                  <span class="prov-rest-why">${explainCooldown({ reason: rest.reason, until: rest.until, status: statusOf(rest) }, now)}</span>
                </li>
              `,
            )}
          </ul>
        </div>
      `}

      ${credential.last_error && html`<${LastError} error=${credential.last_error} now=${now} />`}

      <div class="prov-cred-foot">
        <span class="faint">id</span>
        <span class="mono faint prov-break">${credential.id}</span>
        <${CopyButton} value=${credential.id} label="Copy credential id" />
        <span class="faint">weight <span class="num">${formatNumber(credential.weight)}</span></span>
        <span class="faint">priority <span class="num">${formatNumber(credential.priority)}</span></span>
        ${credential.proxy && html`<span class="faint">proxy <span class="mono prov-break">${credential.proxy}</span></span>`}
        ${credential.service_account_file && html`<span class="faint">service account <span class="mono prov-break">${credential.service_account_file}</span></span>`}
      </div>
    </li>
  `;
}

function CredentialsTab({ provider, health, now, onPatch, onRefresh, onEdit }) {
  const credentials = provider.credentials ?? [];
  if (credentials.length === 0) {
    return html`
      <${EmptyState}
        compact
        icon="key"
        title="No credentials"
        description=${`${provider.name} has no key to call the provider with, so it serves nothing. Add one in the editor.`}
        action=${html`<${Button} icon="edit" onClick=${onEdit}>Edit provider<//>`}
      />
    `;
  }
  return html`
    <ul class="prov-creds">
      ${credentials.map((credential, index) => html`<${CredentialItem} key=${credential.id} credential=${credential} state=${health.states[index]} now=${now} onPatch=${onPatch} onRefresh=${onRefresh} />`)}
    </ul>
  `;
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/**
 * Where the provider's model list stands (its `discovery`), in one line:
 * "42 models, fetched 3m ago", "Fetching the model list…", or that it could
 * not be fetched, why, and the button that asks the upstream again.
 */
function DiscoveryLine({ provider, now, onFetch, fetching }) {
  const listing = discoveryInfo(provider, now);
  if (listing.state === 'pending') return html`<${StatusLamp} tone="info" label=${listing.text} />`;
  if (listing.state !== 'failed') return html`<span>${listing.text}</span>`;
  return html`
    <div class="prov-listing">
      <${StatusLamp} tone="caution" label=${listing.text} detail=${listing.at != null ? formatRelativeTime(listing.at, now) : undefined} />
      <span class="prov-listing-why prov-break">
        ${listing.error ? sentence(listing.error) : 'The gateway gave no reason.'}${listing.models > 0 ? ` The last list that arrived (${plural(listing.models, 'model')}) is still served.` : ''}
      </span>
      <${Button} size="sm" icon="refresh" loading=${fetching} onClick=${() => onFetch(provider)}>Retry<//>
    </div>
  `;
}

/** Where the models a provider serves come from, as a sentence for the Models tab. */
function modelSource(provider, listing) {
  const explicit = provider.config?.models?.length ?? 0;
  if (explicit > 0) return `An explicit list of ${plural(explicit, 'model')}.`;
  if (provider.kind === 'mock') return 'The mock models built into the gateway.';
  // What stands in while there is no list from the upstream: the last one
  // that did arrive, else the built-in catalog, where the kind has one.
  const standIn = listing.models > 0 ? `the last list that arrived (${plural(listing.models, 'model')}) is served` : (provider.model_count ?? 0) > 0 ? 'the built-in catalog for this kind stands in' : 'no models are served';
  if (listing.state === 'ok') return `The provider's own model list: ${listing.text}.`;
  if (listing.state === 'pending') return `The gateway is fetching the provider's model list. Until it arrives, ${standIn}.`;
  if (listing.state === 'failed') return `The provider's model list could not be fetched, so ${standIn}.`;
  if (provider.enabled === false) return 'The provider is disabled: its model list is not fetched.';
  return 'The built-in catalog for this kind: discovery is off.';
}

function ModelsTab({ provider, health, now, onFetchModels, fetchingModels }) {
  // The route table knows which upstream model stands behind each name.
  const table = useResource('/models', { pollMs: 30_000 });
  const [filter, setFilter] = useState('');

  // Which credentials can take a model right now comes from the provider's
  // own view, which live frames keep current; the route table is fetched for
  // the names, and again when what rests changes, in case routes moved too.
  const restSignature = health.states.map((s) => `${s.key}:${s.resting.map((m) => m.model).join(',')}`).join('|');
  const signatureBefore = useRef(restSignature);
  useEffect(() => {
    if (signatureBefore.current === restSignature) return;
    signatureBefore.current = restSignature;
    table.refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [restSignature]);

  /** Credentials ready for an upstream model, and the cooldown that ends first when none is. */
  const availability = (upstream) => {
    const ready = health.states.filter((s) => s.key === 'ready' && !s.resting.some((m) => m.model === upstream)).length;
    const rests = health.states.flatMap((s) => (s.key === 'ready' ? s.resting.filter((m) => m.model === upstream) : s.until != null ? [{ until: s.until, reason: s.reason }] : []));
    rests.sort((a, b) => a.until - b.until);
    return { ready, total: health.total, rest: rests[0] ?? null };
  };

  const rows = useMemo(() => {
    const served = provider.models ?? [];
    if (!Array.isArray(table.data)) return served.map((name) => ({ name, upstream: null, total: null, available: null }));
    const out = [];
    const seen = new Set();
    for (const entry of table.data) {
      for (const route of entry.routes ?? []) {
        if (route.provider !== provider.name) continue;
        seen.add(entry.name);
        out.push({
          name: entry.name,
          upstream: route.upstream_model,
          total: route.credentials_total,
          available: route.credentials_available,
          alias: Array.isArray(entry.alias_targets),
          hidden: entry.hidden === true,
          display: entry.info?.display_name ?? null,
        });
      }
    }
    // Names the provider lists that the route table does not have (yet).
    for (const name of served) if (!seen.has(name)) out.push({ name, upstream: null, total: null, available: null });
    return out;
  }, [table.data, provider.models, provider.name]);

  const needle = filter.trim().toLowerCase();
  const shown = needle ? rows.filter((r) => r.name.toLowerCase().includes(needle) || (r.upstream ?? '').toLowerCase().includes(needle)) : rows;

  const columns = [
    {
      key: 'name',
      header: 'Clients ask for',
      primary: true,
      sortable: true,
      render: (r) => html`
        <span class="prov-model-name">
          <span class="mono prov-break">${r.name}</span>
          ${r.alias && html`<${Badge} outline>virtual model<//>`}
          ${r.hidden && html`<${Badge} outline title="Hidden from client listings; still routable">hidden<//>`}
        </span>
      `,
    },
    {
      key: 'upstream',
      header: 'Upstream model',
      sortable: true,
      render: (r) => (r.upstream ? html`<span class="mono prov-break">${r.upstream}</span>` : DASH),
    },
    {
      key: 'available',
      header: 'Credentials',
      render: (r) => {
        if (r.total == null) return DASH;
        if (provider.enabled === false) return html`<${StatusLamp} tone="off" label="Provider disabled" />`;
        const { ready, total, rest } = availability(r.upstream);
        if (total === 0) return html`<${StatusLamp} tone="stop" label="No credentials" />`;
        if (ready > 0) return html`<${StatusLamp} tone="clear" label=${`${ready} of ${total} ready`} />`;
        // Nothing can take it: say why and until when, if a cooldown is the reason.
        return rest
          ? html`<span class="prov-model-rest"><${StatusLamp} tone="caution" label="Resting" /><span class="prov-model-rest-why">${explainCooldown({ reason: rest.reason, until: rest.until }, now)}</span></span>`
          : html`<${StatusLamp} tone="caution" label="None ready" />`;
      },
    },
  ];

  const listing = discoveryInfo(provider, now);
  const source = modelSource(provider, listing);

  // Why there is nothing to list, by what the gateway says about the model list.
  let nothing;
  if (provider.enabled === false) {
    nothing = { title: 'No models', description: 'A disabled provider serves nothing.' };
  } else if (listing.state === 'pending') {
    nothing = { title: 'Fetching the model list', description: 'The gateway is asking the provider for its models. They appear here when the list has arrived.' };
  } else if (listing.state === 'failed') {
    nothing = {
      title: 'No models',
      description: 'Fetch the model list again, or add models to the explicit list in the editor.',
      action: html`<${Button} size="sm" icon="refresh" loading=${fetchingModels} onClick=${() => onFetchModels(provider)}>Fetch the model list again<//>`,
    };
  } else if (listing.state === 'ok') {
    nothing = { title: 'No models', description: 'The provider lists no models, or its exclude patterns hide them all.' };
  } else {
    nothing = { title: 'No models', description: 'The provider lists no models. Add them to its explicit list, or switch discovery on and fetch the list.' };
  }

  return html`
    <div class="stack" style="--gap:var(--space-3)">
      <p class="prov-note">
        ${source}${provider.prefix ? html` With the prefix, each model is served as <span class="mono">${provider.prefix}/name</span> and under its bare name.` : ''}
      </p>
      ${rows.length > 8 && html`<${Input} size="sm" icon="search" type="search" aria-label="Filter models" placeholder="Filter by client or upstream name" value=${filter} onChange=${setFilter} />`}
      ${table.error && !table.data && html`<${Notice} tone="caution" title="Upstream names are not available">${table.error.message}<//>`}
      <div class="prov-models-table">
        <${Table}
          dense
          sticky=${false}
          rowKey=${(r) => `${r.name}\u0000${r.upstream ?? ''}`}
          columns=${columns}
          rows=${shown}
          loading=${table.loading && rows.length === 0}
          defaultSort=${{ key: 'name', dir: 'asc' }}
          caption=${`Models served by ${provider.name}`}
          empty=${needle
            ? { icon: 'search', title: 'No model matches', description: `Nothing served by ${provider.name} contains "${filter.trim()}".` }
            : { icon: 'models', ...nothing }}
        />
      </div>
    </div>
  `;
}

// ---------------------------------------------------------------------------
// Drawer
// ---------------------------------------------------------------------------

function DetailBody({ provider, now, tab, onTab, onEdit, onPatch, onRefresh, onToggle, toggling, onFetchModels, fetchingModels }) {
  const health = providerHealth(provider, now);
  const info = kindInfo(provider.kind);
  const headerNames = Object.keys(provider.headers ?? {});
  const current = tab === 'models' ? 'models' : 'credentials';

  return html`
    <div class="prov-detail">
      <div class="prov-detail-top">
        <${StatusLamp} size="lg" tone=${health.tone} label=${health.label} detail=${health.detail} pulse=${health.rank === 4 && health.lastUsedAt != null && now - health.lastUsedAt < 10_000} />
        <${Switch}
          label="Enabled"
          checked=${provider.enabled}
          disabled=${toggling}
          onChange=${(next) => onToggle(provider, next)}
          hint=${provider.enabled ? undefined : 'Skipped by the router until it is enabled.'}
        />
      </div>

      <${KeyValue}
        items=${[
          { label: 'Kind', value: html`<span class="row row-wrap">${info.label} <${Badge} mono>${provider.kind}<//></span>` },
          {
            label: 'Base URL',
            value: html`<span class="mono prov-break">${provider.effective_base_url}</span>${!provider.base_url && provider.effective_base_url ? html` <span class="faint">default</span>` : ''}`,
            copy: provider.effective_base_url || false,
          },
          { label: 'Prefix', value: provider.prefix ? html`<span class="mono">${provider.prefix}/</span>` : null },
          { label: 'Priority', value: html`<span class="num">${formatNumber(provider.priority)}</span>` },
          { label: 'Speaks', value: html`<span class="row row-wrap" style="--gap:var(--space-1)">${(provider.protocols ?? []).map((p) => html`<${Badge} mono key=${p}>${p}<//>`)}</span>` },
          { label: 'Proxy', value: provider.proxy ? html`<span class="mono prov-break">${provider.proxy}</span>` : null, hidden: !provider.proxy },
          { label: 'Headers', value: html`<span class="mono prov-break">${headerNames.join(', ')}</span>`, hidden: headerNames.length === 0 },
          { label: 'Model list', value: html`<${DiscoveryLine} provider=${provider} now=${now} onFetch=${onFetchModels} fetching=${fetchingModels} />` },
          {
            label: 'Traffic',
            value:
              health.requests > 0
                ? html`<span><span class="num">${formatNumber(health.requests)}</span> ${health.requests === 1 ? 'request' : 'requests'}, <span class="num">${formatPercent(health.failureRatio)}</span> failed${health.latency != null ? html`, <span class="num">${formatDuration(health.latency)}</span> average latency` : ''}</span>`
                : 'No requests since the gateway started',
          },
        ]}
      />

      <${TestPanel} key=${provider.name} provider=${provider} onTested=${onRefresh} />

      <div class="stack" style="--gap:var(--space-4)">
        <${Tabs}
          label=${`Details of ${provider.name}`}
          value=${current}
          onChange=${onTab}
          tabs=${[
            { id: 'credentials', label: 'Credentials', count: (provider.credentials ?? []).length },
            { id: 'models', label: 'Models', count: (provider.models ?? []).length },
          ]}
        />
        ${current === 'credentials'
          ? html`<${CredentialsTab} provider=${provider} health=${health} now=${now} onPatch=${onPatch} onRefresh=${onRefresh} onEdit=${onEdit} />`
          : html`<${ModelsTab} key=${provider.name} provider=${provider} health=${health} now=${now} onFetchModels=${onFetchModels} fetchingModels=${fetchingModels} />`}
      </div>
    </div>
  `;
}

/**
 * name      the provider named in the URL ('' closes the drawer)
 * provider  its view from the list, when it is there
 * state     "loading" | "error" | "ready": of the list itself
 */
export default function ProviderDetail({ name, provider, state, error, now, tab, onTab, onClose, onEdit, onDelete, onPatch, onRefresh, onRetry, onToggle, toggling, onFetchModels, fetchingModels }) {
  // Keep the last provider on screen while the drawer slides out.
  const last = useRef(null);
  if (provider) last.current = provider;
  const shown = provider ?? (name ? null : last.current);
  const title = shown?.name ?? name ?? 'Provider';

  let body;
  if (shown) {
    body = html`<${DetailBody} provider=${shown} now=${now} tab=${tab} onTab=${onTab} onEdit=${() => onEdit(shown)} onPatch=${onPatch} onRefresh=${onRefresh} onToggle=${onToggle} toggling=${toggling} onFetchModels=${onFetchModels} fetchingModels=${fetchingModels} />`;
  } else if (state === 'loading') {
    body = html`<div class="stack"><${Skeleton} width="40%" height="20px" /><${Skeleton} lines=${5} /><${Skeleton} lines=${4} /></div>`;
  } else if (state === 'error') {
    body = html`<${ErrorState} title="Could not load the providers" error=${error} onRetry=${onRetry} />`;
  } else {
    body = html`
      <${EmptyState}
        icon="providers"
        title=${`No provider named ${name}`}
        description="It may have been renamed or deleted, here or in the configuration file."
        action=${html`<${Button} onClick=${onClose}>Back to the list<//>`}
      />
    `;
  }

  return html`
    <${Drawer}
      open=${!!name}
      onClose=${onClose}
      width="720px"
      title=${title}
      subtitle=${shown?.effective_base_url}
      class="prov-drawer"
      actions=${shown &&
      html`
        <${Button} size="sm" icon="edit" onClick=${() => onEdit(shown)}>Edit<//>
        <${Menu} label=${`Actions for ${shown.name}`} items=${[{ label: 'Delete provider', icon: 'trash', danger: true, onSelect: () => onDelete(shown) }]} />
      `}
    >
      ${body}
    <//>
  `;
}
