// Settings (#/settings): everything in switchyard.toml that is not a
// provider, a client key or an alias, plus the file itself.
//
// One tab is open at a time and its id is in the URL (#/settings?tab=routing).
// The four form tabs edit the scalar sections through PATCH /settings and
// share one copy of GET /config, held here; payload rules, prices and the
// raw file load their own documents. The modules are in ./settings/:
//
//   common.js     save bar, unsaved-changes guard, rows, list drafts
//   general.js    server, client authentication, admin interface
//   routing.js    strategy, retries, cooldowns
//   streaming.js  streaming, proxy, timeouts
//   logging.js    application log, request bodies, usage
//   payload.js    payload rules
//   exact.js      reading the rules with every number kept as written
//   pricing.js    prices
//   raw.js        the raw file: validate, diff, save, reload

import { html, useEffect, useRef, useState } from '../../vendor/preact-htm.js';
import { Button, ErrorState, Notice, Page, Panel, Tabs, toast } from '../components/index.js';
import { api } from '../lib/api.js';
import { useCommands } from '../lib/commands.js';
import { loadStyles, prefersReducedMotion } from '../lib/dom.js';
import { formatTime } from '../lib/format.js';
import { useAsync, useResource } from '../lib/hooks.js';
import { liveState, useLive } from '../lib/live.js';
import { navigate, useQueryParam } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import { FormSkeleton, confirmLeave, secretSettled, sentence } from './settings/common.js';
import { GeneralTab } from './settings/general.js';
import { LoggingTab } from './settings/logging.js';
import { PayloadTab } from './settings/payload.js';
import { PricingTab } from './settings/pricing.js';
import { RawTab } from './settings/raw.js';
import { RoutingTab } from './settings/routing.js';
import { StreamingTab } from './settings/streaming.js';

await loadStyles('pages/settings.css');

const TABS = [
  { id: 'general', label: 'General', keywords: 'server host port tls cors body limit data directory admin secret remote access authentication' },
  { id: 'routing', label: 'Routing', keywords: 'strategy round robin fill first weighted least latency affinity attempts cooldown' },
  { id: 'streaming', label: 'Streaming and upstream', keywords: 'keep-alive idle timeout bootstrap proxy socks connect request timeout headers' },
  { id: 'logging', label: 'Logging and usage', keywords: 'log level file request bodies capture privacy usage retention' },
  { id: 'payload', label: 'Payload rules', keywords: 'default override filter json body patch' },
  { id: 'pricing', label: 'Pricing', keywords: 'prices cost usd tokens' },
  { id: 'raw', label: 'Raw file', keywords: 'toml switchyard.toml editor validate reload' },
];

const FORM_TABS = { general: GeneralTab, routing: RoutingTab, streaming: StreamingTab, logging: LoggingTab };

export default function Settings() {
  const [tabParam, setTab] = useQueryParam('tab', 'general');
  const tab = TABS.some((entry) => entry.id === tabParam) ? tabParam : 'general';

  // Live frames keep the forms fresh; without the live connection, poll.
  const liveOpen = useStore(liveState, (s) => s.status === 'open');
  const config = useResource('/config', { pollMs: liveOpen ? 0 : 20_000 });
  const status = useResource('/status');
  const [rejected, setRejected] = useState(null);

  useLive('config.reloaded', (data) => {
    setRejected(data && data.ok === false ? data : null);
    // After a change of the admin secret, wait until this session uses it.
    secretSettled().then(() => {
      config.refresh();
      status.refresh();
    });
  });

  // Frames sent while the connection was down are gone: refetch when it returns.
  const wasOpen = useRef(liveOpen);
  useEffect(() => {
    if (liveOpen && !wasOpen.current) config.refresh();
    wasOpen.current = liveOpen;
  }, [liveOpen]);

  useCommands(
    () =>
      TABS.map((entry) => ({
        id: `settings:${entry.id}`,
        label: `Settings: ${entry.label.toLowerCase()}`,
        group: 'Settings',
        icon: 'settings',
        keywords: entry.keywords,
        run: () => navigate('/settings', { query: { tab: entry.id === 'general' ? null : entry.id } }),
      })),
    [],
  );

  const change = async (next) => {
    if (next !== tab && (await confirmLeave())) setTab(next);
  };

  // Seven tabs do not fit a phone: the strip scrolls sideways. Keep the
  // selected tab in view, so a link to a later tab shows which one is open.
  const main = useRef(null);
  const firstScroll = useRef(true);
  useEffect(() => {
    const strip = main.current?.querySelector(':scope > .tabs');
    const selected = strip?.querySelector('[role="tab"][aria-selected="true"]');
    const first = firstScroll.current;
    firstScroll.current = false;
    if (!strip || !selected || strip.scrollWidth <= strip.clientWidth) return;
    const box = strip.getBoundingClientRect();
    const at = selected.getBoundingClientRect();
    const left = strip.scrollLeft + at.left - box.left - (box.width - at.width) / 2;
    strip.scrollTo({ left: Math.max(0, left), behavior: first || prefersReducedMotion() ? 'auto' : 'smooth' });
  }, [tab]);

  // The gateway announces a refused file, but not that the file was put back
  // the way it was: reloading is how the notice is cleared then.
  const reload = useAsync(() => api.post('/reload'));
  const reloadFromDisk = async () => {
    const result = await reload.run();
    if (result) {
      setRejected(null);
      config.mutate(result);
      toast.success('Reloaded from disk', { description: 'The file is valid and in effect.' });
    }
  };
  useEffect(() => {
    if (reload.error) setRejected({ ok: false, message: reload.error.message, at: Date.now() });
  }, [reload.error]);

  const restart = config.data?.restart_required ?? [];
  const FormTab = FORM_TABS[tab];

  let body;
  if (FormTab) {
    if (config.data) body = html`<${FormTab} key=${tab} config=${config} status=${status.data} />`;
    else if (config.error) body = html`<${Panel} flush><${ErrorState} title="Could not load the settings" error=${config.error} onRetry=${config.refresh} /><//>`;
    else body = html`<div class="settings-form"><${FormSkeleton} rows=${6} /><${FormSkeleton} rows=${3} /></div>`;
  } else if (tab === 'payload') body = html`<${PayloadTab} key="payload" />`;
  else if (tab === 'pricing') body = html`<${PricingTab} key="pricing" />`;
  else {
    // `refused` is the refusal itself, so the tab can tell a new one from the
    // one it has already looked into; it reports back which one was resolved.
    body = html`<${RawTab} key="raw" onConfig=${config.mutate} refused=${rejected} onValid=${(resolved) => setRejected((current) => (current === resolved ? null : current))} />`;
  }

  return html`
    <${Page} class="settings" title="Settings" description="Gateway-wide configuration, saved to switchyard.toml and applied at once. Providers, API keys and aliases have their own pages.">
      <div class="settings-main" ref=${main}>
      ${rejected &&
      html`<${Notice}
        tone="stop"
        title="The configuration file on disk was refused"
        action=${tab === 'raw'
          ? null
          : html`<div class="btn-group">
              <${Button} size="sm" onClick=${() => change('raw')}>Open the raw file<//>
              <${Button} size="sm" icon="refresh" loading=${reload.loading} onClick=${reloadFromDisk}>Reload from disk<//>
            </div>`}
      >
        ${sentence(rejected.message || 'The file is not a valid configuration')} The gateway keeps running on the last valid configuration. Fix the file, then reload it. Refused at ${formatTime(rejected.at)}.
      <//>`}
      ${restart.length > 0 &&
      html`<${Notice} tone="caution" title="Restart needed">
        Saved, but only applied when the gateway restarts: ${restart.map((setting, i) => html`${i > 0 ? ', ' : ''}<span class="mono" key=${setting}>${setting}</span>`)}. Until then it keeps running with the previous ${restart.length === 1 ? 'value' : 'values'}.
      <//>`}
      ${config.error &&
      config.data &&
      FormTab &&
      html`<${Notice} tone="caution" title="Could not refresh the settings" action=${html`<${Button} size="sm" icon="refresh" onClick=${config.refresh}>Try again<//>`}>
        ${sentence(config.error.message)} The values below are the last ones loaded.
      <//>`}

      <${Tabs} label="Settings sections" tabs=${TABS} value=${tab} onChange=${change} />

      <div class="settings-body" role="tabpanel" aria-label=${TABS.find((entry) => entry.id === tab).label} data-tab=${tab}>${body}</div>
      </div>
    <//>
  `;
}
