// Overview, top of the page: the status strip, the notices that need a
// decision (a refresh that failed, restart needed, configuration warnings)
// and the setup steps of a new installation.

import { html, useMemo, useState } from '../../../vendor/preact-htm.js';
import { Button, CodeBlock, CopyButton, IconButton, Notice, Panel, Skeleton, StatusLamp } from '../../components/index.js';
import { formatDateTime, formatDuration, formatNumber, formatTime, plural, sentence } from '../../lib/format.js';
import { href } from '../../lib/router.js';
import { serverClock, useServerNow } from './data.js';
import { clientBase, curlExample, firstRun, gatewayVerdict, summarizeProvider, warningTarget } from './model.js';
import { focusSoon, focusTarget } from './stale.js';

// ---------------------------------------------------------------------------
// Status strip
// ---------------------------------------------------------------------------

function Fact({ label, loading, wide = false, count = false, children }) {
  return html`
    <div class="overview-fact" data-wide=${wide ? '' : undefined} data-count=${count ? '' : undefined}>
      <dt class="plate-label">${label}</dt>
      <dd>${loading ? html`<${Skeleton} width="72px" height="14px" />` : children}</dd>
    </div>
  `;
}

/**
 * One line that answers "is it healthy, what is it, where is it". While the
 * configuration file on disk is refused the verdict says so and links to the
 * raw file (see gatewayVerdict).
 * status     /status (useResource)
 * providers  /providers (useResource)
 * down       null, or { at, unreachable } while the gateway does not answer:
 *            `at` is when it last did (this browser's clock), `unreachable`
 *            is true when no response arrived at all
 */
export function StatusStrip({ status, providers, down = null }) {
  const now = useServerNow(1000);
  const data = status.data;
  const summaries = useMemo(() => (providers.data ? providers.data.map((p) => summarizeProvider(p, now)) : null), [providers.data, now]);
  // What the page last heard is not the state of a gateway that has stopped
  // answering: the verdict says so, and nothing pulses or counts on.
  const verdict = down
    ? { tone: 'stop', label: down.unreachable ? 'Not reachable' : 'Not answering', detail: `Last answer at ${formatTime(down.at)}, trying again every 5s` }
    : gatewayVerdict(summaries, data, Boolean(providers.error) && !providers.data);
  const loading = !data;
  const counts = data?.counts;
  const uptime = data ? Math.max(0, (down ? down.at + serverClock.get().offset : now) - data.started_at) : null;

  return html`
    <section class="overview-status" aria-label="Gateway status" data-down=${down ? '' : undefined}>
      <div class="overview-verdict" role="status">
        ${verdict
          ? html`
              <${StatusLamp} tone=${verdict.tone} size="lg" pulse=${verdict.tone === 'clear' && !down} title=${verdict.label} />
              <div class="overview-verdict-text">
                <span class="overview-verdict-label">${verdict.label}</span>
                <span class="overview-verdict-detail">${verdict.detail}</span>
                ${verdict.refused &&
                html`<span class="overview-verdict-refused">The file on disk was refused. <a href=${href('/settings', { tab: 'raw' })}>Open the raw file</a></span>`}
              </div>
            `
          : html`<${Skeleton} width="160px" height="20px" />`}
      </div>
      <dl class="overview-facts">
        <${Fact} label="Version" loading=${loading}><span class="mono">${data?.version}</span><//>
        <${Fact} label=${down ? 'Uptime at last answer' : 'Uptime'} loading=${loading}>
          <span class=${down ? 'num faint' : 'num'} title=${data ? `Started ${formatDateTime(data.started_at)}` : undefined}>${uptime != null && uptime < 60_000 ? `${Math.floor(uptime / 1000)}s` : formatDuration(uptime)}</span>
        <//>
        <${Fact} label="Listening on" loading=${loading} wide>
          ${data?.listen
            ? html`<span class="overview-listen"><span class="mono">${data.listen}</span><${CopyButton} value=${data.listen} label="Copy listen address" /></span>`
            : html`<span class="faint">Not known</span>`}
        <//>
        <${Fact} label="Providers" loading=${loading} count><a class="num" href=${href('/providers')} aria-label=${plural(counts?.providers ?? 0, 'provider')}>${formatNumber(counts?.providers)}</a><//>
        <${Fact} label="Models" loading=${loading} count><a class="num" href=${href('/models')} aria-label=${plural(counts?.models ?? 0, 'model')}>${formatNumber(counts?.models)}</a><//>
        <${Fact} label="Client keys" loading=${loading} count><a class="num" href=${href('/keys')} aria-label=${plural(counts?.client_keys ?? 0, 'client key')}>${formatNumber(counts?.client_keys)}</a><//>
      </dl>
    </section>
  `;
}

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

/** `provider \`x\`: ...` with the quoted names set in mono. */
function WarningText({ text }) {
  const parts = String(text).split(/`([^`]+)`/);
  return html`<span class="overview-warning-text">${parts.map((part, i) => (i % 2 === 1 ? html`<span class="mono" key=${i}>${part}</span>` : part))}</span>`;
}

const WARNINGS_SHOWN = 4;

/** "a", "a and b", "a, b and c". */
function listed(names) {
  if (names.length < 2) return names.join('');
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`;
}

/**
 * One notice for everything that could not be refreshed. The sections keep
 * their last data and each says since when; this says why, and retries.
 *
 * down      see StatusStrip: the gateway does not answer at all
 * sections  [{ what, resource }] whose last refetch failed with data on screen
 * onRetry   refetch them
 */
function RefreshNotice({ down, sections, onRetry }) {
  if (!down && sections.length === 0) return null;
  const retrying = sections.some((section) => section.resource.refreshing);
  const action = html`<${Button} size="sm" icon="refresh" loading=${retrying} onClick=${onRetry}>Try again<//>`;
  // The gateway's own words, as a sentence: other text follows them.
  const why = sentence(sections[0]?.resource.error?.message);
  if (down) {
    return html`
      <${Notice} tone="stop" title=${down.unreachable ? 'The gateway cannot be reached' : 'The gateway is not answering'} action=${action}>
        ${why} Everything on this page shows what was loaded up to <span class="num">${formatTime(down.at)}</span>.${!down.unreachable && ' The gateway is reachable but answers with an error: its own log says why.'}
      <//>
    `;
  }
  // Old data on screen, or (the request records) none at all.
  const stale = sections.filter((section) => section.resource.data != null);
  const missing = sections.filter((section) => section.resource.data == null);
  const subjectOf = (names) => {
    const subject = listed(names);
    return `${subject.charAt(0).toUpperCase()}${subject.slice(1)}`;
  };
  const since = Math.min(...stale.map((section) => section.resource.updatedAt ?? Infinity));
  const title =
    sections.length > 1 ? 'Could not refresh parts of this page' : stale.length === 1 ? `Could not refresh ${stale[0].what}` : `Could not load ${missing[0].what}`;
  return html`
    <${Notice} tone="caution" title=${title} action=${action}>
      ${why}${stale.length > 0 &&
      html`${' '}${sections.length === 1 ? 'Showing' : `${subjectOf(stale.map((section) => section.what))} ${stale.length === 1 ? 'shows' : 'show'}`} what was loaded at <span class="num">${Number.isFinite(since) ? formatTime(since) : 'an earlier time'}</span>.`}${missing.length > 0 &&
      sections.length > 1 &&
      html`${' '}${subjectOf(missing.map((section) => section.what))} could not be loaded.`}
    <//>
  `;
}

/**
 * What needs a decision: a refresh that failed, a pending restart,
 * configuration warnings. Warnings can be dismissed one by one; a dismissed
 * warning stays away until its text changes, and can be brought back.
 *
 * down, sections, onRetry  see RefreshNotice
 * dismissed, setDismissed  the dismissed warning texts (localStorage)
 */
export function Notices({ status, down = null, sections = [], onRetry, dismissed, setDismissed }) {
  const [all, setAll] = useState(false);
  const data = status.data;
  if (!data) return null;

  const restart = data.restart_required ?? [];
  const warnings = data.warnings ?? [];
  const hidden = new Set(dismissed);
  const open = warnings.filter((w) => !hidden.has(w));
  const shown = all ? open : open.slice(0, WARNINGS_SHOWN);
  const dismiss = (texts, index) => {
    // Only texts that are still reported are remembered.
    setDismissed([...new Set([...dismissed, ...texts])].filter((w) => warnings.includes(w)));
    // The button that was pressed goes with its warning: the focus moves to
    // the next warning's button, or to the way back when none is left.
    focusSoon(() => focusTarget('dismiss', index) ?? focusTarget('show-warnings'));
  };

  return html`
    <${RefreshNotice} down=${down} sections=${sections} onRetry=${onRetry} />
    ${restart.length > 0 &&
    html`
      <${Notice} tone="caution" title="Restart needed" action=${html`<${Button} size="sm" href=${href('/settings')}>Open settings<//>`}>
        ${restart.length === 1 ? 'This setting was changed and takes' : 'These settings were changed and take'} effect when the gateway restarts:${' '}
        ${restart.map((name, i) => html`${i > 0 ? ', ' : ''}<span class="mono" key=${name}>${name}</span>`)}. Until then the gateway runs on the old ${restart.length === 1 ? 'value' : 'values'}.
      <//>
    `}
    ${open.length > 0 &&
    html`
      <${Notice} tone="caution" title=${open.length === 1 ? '1 configuration warning' : `${formatNumber(open.length)} configuration warnings`}>
        <ul class="overview-warnings">
          ${shown.map((text, index) => {
            const target = warningTarget(text);
            return html`
              <li key=${text}>
                <${WarningText} text=${text} />
                <span class="overview-warning-actions">
                  <a href=${href(target.path, target.query)}>${target.label}</a>
                  <${IconButton} icon="x" size="sm" label="Dismiss this warning" data-overview-focus="dismiss" onClick=${() => dismiss([text], index)} />
                </span>
              </li>
            `;
          })}
        </ul>
        ${open.length > 1 &&
        html`<div class="overview-warnings-more">
          ${open.length > WARNINGS_SHOWN &&
          html`<${Button} size="sm" variant="ghost" aria-expanded=${all ? 'true' : 'false'} onClick=${() => setAll(!all)}>
            ${all ? 'Show fewer' : `Show all ${formatNumber(open.length)} warnings`}
          <//>`}
          <${Button} size="sm" variant="ghost" onClick=${() => dismiss(open, 0)}>Dismiss all<//>
        </div>`}
      <//>
    `}
  `;
}

/**
 * What the operator put away, and the way to get it back.
 *
 * warnings             how many warnings are dismissed
 * setup                the setup steps are hidden
 * onWarnings, onSetup  bring them back
 */
export function PutAway({ warnings, setup, onWarnings, onSetup }) {
  if (warnings === 0 && !setup) return null;
  // "Show again" removes itself: the focus follows what it brought back.
  const showWarnings = () => {
    onWarnings();
    focusSoon(() => focusTarget('dismiss'));
  };
  const showSetup = () => {
    onSetup();
    focusSoon(() => focusTarget('hide-setup'));
  };
  return html`
    <div class="overview-hidden">
      ${warnings > 0 && html`<span>${plural(warnings, 'warning')} dismissed <${Button} size="sm" variant="ghost" data-overview-focus="show-warnings" onClick=${showWarnings}>Show again<//></span>`}
      ${setup && html`<span>Setup steps hidden <${Button} size="sm" variant="ghost" data-overview-focus="show-setup" onClick=${showSetup}>Show again<//></span>`}
    </div>
  `;
}

// ---------------------------------------------------------------------------
// First run
// ---------------------------------------------------------------------------

function Step({ done, title, action, children }) {
  return html`
    <li class="overview-step" data-done=${done ? '' : undefined}>
      <div class="overview-step-head">
        <h3>${title}</h3>
        <${StatusLamp} tone=${done ? 'clear' : 'off'} label=${done ? 'Done' : 'To do'} />
      </div>
      <div class="overview-step-body">${children}</div>
      ${action && html`<div class="overview-step-action">${action}</div>`}
    </li>
  `;
}

/**
 * The setup steps of a new installation: shown while only the mock provider
 * exists or no client key does. `onHide` puts them away.
 */
export function FirstRun({ status, providers, onHide }) {
  const state = firstRun(status.data, providers.data);
  if (!state.show) return null;
  const listen = status.data.listen;
  const { base, direct } = clientBase(listen, typeof location === 'undefined' ? null : location, status.data.tls);
  const curl = curlExample({ base, model: state.model, authRequired: state.authRequired });
  const providerDone = !state.noProviders && !state.mockOnly;
  // The dashboard was not opened on the socket the gateway listens on (a
  // proxy, a port mapping): say which address the example uses and why.
  const viaPage = !direct && Boolean(listen);
  // The X removes the whole panel: the focus goes to the way back.
  const hide = () => {
    onHide();
    focusSoon(() => focusTarget('show-setup'));
  };

  return html`
    <${Panel}
      title="Set up the gateway"
      description="What is left before your own clients are served."
      actions=${html`<${IconButton} icon="x" label="Hide the setup steps" data-overview-focus="hide-setup" onClick=${hide} />`}
    >
      <ol class="overview-steps">
        <${Step}
          done=${providerDone}
          title="1. Add a provider"
          action=${html`<${Button} size="sm" variant=${providerDone ? 'secondary' : 'primary'} icon="plus" href=${href('/providers', { new: 1 })}>Add a provider<//>`}
        >
          ${state.noProviders
            ? 'No provider is configured, so every request is refused. Add an upstream and its API key.'
            : state.mockOnly
              ? 'Only the built-in mock provider is configured. It answers with canned text and needs no key, so clients and this dashboard can be tried before a real upstream exists.'
              : 'A real provider is configured.'}
        <//>
        <${Step}
          done=${!state.noKey}
          title="2. Create a client key"
          action=${html`<${Button} size="sm" icon="key" href=${href('/keys', state.noKey ? { new: 1 } : undefined)}>${state.noKey ? 'Create a client key' : 'Open API keys'}<//>`}
        >
          ${state.noKey
            ? state.authRequired
              ? 'No client key exists, so the gateway answers every client with 401. Create one for each application.'
              : html`No client key exists. Clients are admitted without one because <span class="mono">auth.required</span> is off.`
            : `${plural(status.data.counts.client_keys, 'client key')} can call the gateway.`}
        <//>
        <${Step} done=${state.hasTraffic} title="3. Send a request" action=${html`<${Button} size="sm" icon="playground" href=${href('/playground')}>Open the playground<//>`}>
          <p>Try a model in the playground, or use the example below in bash or zsh.</p>
          <${CodeBlock}
            class="overview-curl"
            language="text"
            title="Example request for bash/zsh"
            value=${curl}
            note=${state.authRequired || !state.model || viaPage
              ? html`${state.authRequired &&
                  html`Set <span class="mono">SWITCHYARD_KEY</span> to a client key first. A key is shown when it is created and can be revealed again under API keys.${' '}`}${!state.model &&
                  html`Replace <span class="mono">MODEL</span> with a model one of your providers serves.${' '}`}${viaPage &&
                  html`The gateway listens on <span class="mono">${listen}</span>. The example uses the address this dashboard was opened on, because that is the one known to reach the gateway from here.`}`
              : null}
          />
        <//>
      </ol>
    <//>
  `;
}
