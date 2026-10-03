// Models page: the detail drawer of one model or alias. Its routes as a
// track diagram, the full metadata, copy-ready commands for every protocol,
// and the reasoning suffix explained with this model's own limits.

import { html, useMemo, useRef } from '../../../vendor/preact-htm.js';
import {
  Button,
  CodeBlock,
  CopyButton,
  Drawer,
  EmptyState,
  ErrorState,
  KeyValue,
  Notice,
  Segmented,
  Skeleton,
  StatusLamp,
  Switch,
  TrackDiagram,
} from '../../components/index.js';
import { formatDate, formatNumber, plural } from '../../lib/format.js';
import { useLocalStorage } from '../../lib/hooks.js';
import { href } from '../../lib/router.js';
import { PROTOCOLS, curlFor, depthPhrase, gatewayBase, litRoutes, lookup, parseSuffix, protocolFamily, reasoningDetail, suffixExamples, suffixPins, targetsOwnName } from './logic.js';
import { BackIn, KindBadges } from './table.js';

/** Beyond this many routes the diagram is a wall of rails; the list alone reads better. */
const DIAGRAM_MAX_ROUTES = 16;

function Section({ title, description, children }) {
  return html`
    <section class="models-section">
      <div class="models-section-head">
        <h3>${title}</h3>
        ${description && html`<p class="muted">${description}</p>`}
      </div>
      ${children}
    </section>
  `;
}

/** The gateway words reasons as fragments ("environment variable X is not set"): quote them as one sentence. */
const quoted = (text) => `The gateway says: ${String(text).trim().replace(/[.\s]+$/, '')}.`;

/** Why requests for this model fail right now, and what to do about it. */
function AvailabilityNotice({ row, onEditAliases, onRetry }) {
  const key = row.availability.key;
  if (key === 'routable') return null;
  if (key === 'unknown') {
    return html`
      <${Notice} tone="caution" title="Requests for this model fail right now" action=${html`<${Button} size="sm" icon="refresh" onClick=${onRetry}>Try again<//>`}>
        No credential can serve it at the moment. Whether they are cooling down or switched off is not known, because the provider details could not be loaded.
      <//>
    `;
  }
  if (key === 'cooling') {
    const lead = row.routes.length > 1 ? 'Every credential on every route is cooling down after failures. ' : 'Every credential that serves it is cooling down after failures. ';
    // One line: htm drops the spaces around a line break inside running text.
    return html`
      <${Notice} tone="caution" title="Requests for this model fail right now">
        <span>${lead}</span><${BackIn} until=${row.availability.until} prefix="The first one is back in" suffix=". " /><span>Until then clients get an error with a Retry-After header. A cooldown can be cleared on the provider's page.</span>
      <//>
    `;
  }
  if (key === 'noroute') {
    return html`
      <${Notice} tone="caution" title="This alias leads nowhere" action=${html`<${Button} size="sm" onClick=${onEditAliases}>Edit aliases<//>`}>
        None of its targets matches a model the gateway serves, so the alias is ignored and requests for it fail as an unknown model.
      <//>
    `;
  }
  const off = row.availability.tone === 'off';
  // With one route the reason fits here; with several each has its own in the list below.
  const only = row.routes.length === 1 ? row.routes[0] : null;
  const reason = only ? (only.said ? quoted(only.said) : only.why) : 'The reason for each route is in the list below.';
  return html`
    <${Notice} tone=${off ? 'caution' : 'stop'} title=${off ? 'Every route is switched off' : 'No credential can serve this model'}>
      ${off ? 'The providers or credentials behind it are disabled. Enable one on the provider’s page.' : `Requests for it fail until a provider has a working credential.${reason ? ` ${reason}` : ''}`}
    <//>
  `;
}

/** The routes as a list: what the diagram cannot hold (reasons, counts, links). */
function RouteList({ routes, tierOf, lit }) {
  const numbered = routes.length > 1;
  return html`
    <ol class="models-routelist">
      ${routes.map((route) => {
        // On an alias's route: the target it belongs to, as written.
        const target = route.target ?? null;
        return html`
          <li class="models-routelist-item" key=${route.index} data-lit=${lit.has(route.index) ? '' : undefined}>
            <div class="models-routelist-top">
              <span class="models-routelist-name">
                ${numbered && html`<span class="models-targets-order num" title="Order of preference; routes with the same number share requests">${tierOf.get(route.index)}</span>`}
                <a class="mono models-routelist-provider" href=${href('/providers', { open: route.provider })}>${route.provider}</a>
              </span>
              <${StatusLamp}
                tone=${route.state}
                label=${route.label}
                detail=${route.state === 'caution' && (route.until || route.reason) ? html`<${BackIn} until=${route.until} reason=${route.reason} />` : lit.has(route.index) ? 'takes requests now' : null}
              />
            </div>
            <dl class="models-routelist-facts">
              <div><dt>Upstream model</dt><dd class="mono">${route.upstream_model}</dd></div>
              <div><dt>Credentials</dt><dd><span class="num">${route.credentials_available}</span> of <span class="num">${route.credentials_total}</span> available</dd></div>
              ${target && html`<div><dt>Through target</dt><dd class="mono">${target}</dd></div>`}
              ${(route.why || route.said) && html`<div><dt>Reason</dt><dd>${route.why ?? quoted(route.said)}</dd></div>`}
              ${route.unknown && html`<div><dt>Reason</dt><dd>Not known: the provider details could not be loaded.</dd></div>`}
            </dl>
          </li>
        `;
      })}
    </ol>
  `;
}

function Routes({ row }) {
  const { diagram, lit, ordered, tierOf } = useMemo(() => {
    const litSet = litRoutes(row);
    // Routes of one tier share an order number: the gateway spreads requests
    // across them and only moves to the next tier when this one cannot serve.
    // Alias target first, then priority (the tier GET /models gives each
    // route), highest first.
    const tierOfRoute = (r) => [r.targetIndex ?? 0, -(r.priority ?? 0)];
    const byTier = (a, b) => a[0] - b[0] || a[1] - b[1];
    const tiers = [...new Map(row.routes.map((r) => [tierOfRoute(r).join(':'), tierOfRoute(r)])).values()].sort(byTier).map((t) => t.join(':'));
    const tier = new Map(row.routes.map((r) => [r.index, tiers.indexOf(tierOfRoute(r).join(':')) + 1]));
    // Preferred routes first; within a tier, the order the gateway lists them in.
    const sorted = [...row.routes].sort((a, b) => tier.get(a.index) - tier.get(b.index) || a.index - b.index);
    return {
      lit: litSet,
      ordered: sorted,
      tierOf: tier,
      diagram: {
        models: [{ id: row.name, label: row.name, note: row.isAlias ? plural(row.aliasTargets.length, 'target') : undefined }],
        providers: sorted.map((r) => ({
          id: `route-${r.index}`,
          label: r.provider,
          state: r.state,
          note: `${r.upstream_model !== row.name ? `${r.upstream_model}, ` : ''}${r.credentials_available} of ${r.credentials_total} available`,
        })),
        routes: sorted.map((r) => ({ model: row.name, provider: `route-${r.index}`, order: tier.get(r.index), state: r.state, lit: litSet.has(r.index) })),
      },
    };
  }, [row]);

  if (row.routes.length === 0) {
    return html`<p class="muted">No provider serves ${row.isAlias ? 'any target of this alias' : 'this model'} at the moment.</p>`;
  }
  if (row.routes.length > DIAGRAM_MAX_ROUTES) {
    return html`
      <p class="models-caption">${plural(row.routes.length, 'route')}, too many to draw as tracks. They are listed in order of preference; ${plural(lit.size, 'route')} ${lit.size === 1 ? 'takes' : 'take'} requests now.</p>
      <${RouteList} routes=${ordered} tierOf=${tierOf} lit=${lit} />
    `;
  }
  return html`
    <div class="models-track">
      <${TrackDiagram} models=${diagram.models} providers=${diagram.providers} routes=${diagram.routes} selected=${row.name} label=${`Routes of ${row.name}`} />
    </div>
    ${row.routes.length > 1 &&
    html`<p class="models-caption">Lit rails take requests now. Routes with the same number share them by the routing strategy; a higher number takes over only when the ones before it cannot serve.</p>`}
    <${RouteList} routes=${ordered} tierOf=${tierOf} lit=${lit} />
  `;
}

function Details({ row, rowsByName, aliases, onOpen }) {
  const info = row.info;
  const hiddenBy = (aliases ?? []).filter((a) => a.hide_targets && (a.targets ?? []).some((t) => parseSuffix(t).base.toLowerCase() === row.name.toLowerCase() || t.toLowerCase() === row.name.toLowerCase())).map((a) => a.name);
  const reasoning = reasoningDetail(info);
  const targets =
    row.isAlias &&
    html`
      <ol class="models-targets">
        ${row.aliasTargets.map((target, index) => {
          // Whether a target routes is said by GET /models: the alias's
          // routes each name their target. The entry to link to is looked
          // up by name, as the gateway would find it.
          const routed = row.routes.some((route) => route.targetIndex === index);
          // A target that names the alias itself is the provider's model of
          // that name when there is one, and a dead end when not.
          const self = targetsOwnName(row, target);
          const entry = self ? null : (lookup(rowsByName, parseSuffix(target).base) ?? lookup(rowsByName, target));
          return html`
            <li key=${`${index}-${target}`}>
              <span class="models-targets-order num">${index + 1}</span>
              ${entry ? html`<button type="button" class="models-link mono" onClick=${() => onOpen(entry.name)}>${target}</button>` : html`<span class="mono">${target}</span>`}
              ${self && html`<span class="faint">${routed ? 'the provider’s model of the same name' : 'leads back to this alias, ignored'}</span>`}
              ${!self && !routed && html`<span class="faint">${entry ? 'has no route, skipped' : 'matches no model'}</span>`}
            </li>
          `;
        })}
      </ol>
    `;
  return html`
    <${KeyValue}
      items=${[
        { label: 'Name', value: row.name, mono: true, copy: true },
        { label: 'Kind', value: html`<${KindBadges} row=${row} />` },
        { label: 'Targets, in order', value: targets, hidden: !row.isAlias },
        // Optional metadata is left out when the gateway has none; limits stay, as a dash.
        { label: 'Display name', value: info.display_name, hidden: !info.display_name },
        { label: 'Description', value: info.description, hidden: !info.description },
        { label: 'Owned by', value: info.owned_by, mono: true, hidden: !info.owned_by },
        { label: 'Released', value: info.created ? formatDate(info.created) : null, hidden: !info.created },
        { label: 'Context window', value: info.context_window ? `${formatNumber(info.context_window)} tokens` : null },
        { label: 'Max output', value: info.max_output_tokens ? `${formatNumber(info.max_output_tokens)} tokens` : null },
        {
          label: 'Reasoning',
          value: reasoning ? html`<ul class="models-facts">${reasoning.map((line) => html`<li key=${line}>${line}</li>`)}</ul>` : info.known === false ? 'Not known. Reasoning settings are passed through as sent.' : 'None',
        },
        {
          label: 'Metadata',
          value: info.known === false ? 'None. Requests pass through without being fitted to limits.' : row.isAlias ? 'Taken from its first routable target' : 'Known to the gateway',
        },
        {
          label: 'Listed to clients',
          // The gateway leaves an alias it ignores out of the lists, and a name nothing serves with it.
          value: row.ignored
            ? 'No. The gateway ignores an alias that has no routable target.'
            : row.hidden
              ? `No. Hidden by ${hiddenBy.length ? `alias ${hiddenBy.join(', ')}` : 'an alias'}; requests that name it still work.`
              : row.routes.length === 0
                ? 'No. No provider serves it.'
                : 'Yes',
        },
      ]}
    />
  `;
}

function CallIt({ row, status }) {
  const [protocol, setProtocol] = useLocalStorage('models.protocol', 'chat');
  const [stream, setStream] = useLocalStorage('models.stream', false);
  const current = PROTOCOLS.find((p) => p.id === protocol) ?? PROTOCOLS[0];
  const auth = status?.auth_required !== false;
  const base = gatewayBase(status?.listen, typeof location === 'undefined' ? null : location, status?.tls === true);
  const command = curlFor(current.id, { base, model: row.name, stream: !!stream, auth });
  return html`
    <div class="models-call">
      <div class="models-call-controls">
        <${Segmented} label="Protocol" size="sm" value=${current.id} onChange=${setProtocol} options=${PROTOCOLS.map((p) => ({ value: p.id, label: p.short, title: p.label }))} />
        <${Switch} label="Streaming" checked=${!!stream} onChange=${setStream} />
      </div>
      <${CodeBlock}
        language="text"
        title=${`${current.label} with curl`}
        value=${command}
        note=${auth
          ? html`The command reads the client key from <span class="mono">SWITCHYARD_KEY</span>. Keys are on the <a href=${href('/keys')}>API keys</a> page. The address is the one the gateway listens on; behind a reverse proxy use its public URL.`
          : 'This gateway accepts requests without a client key. The address is the one it listens on; behind a reverse proxy use its public URL.'}
      />
    </div>
  `;
}

/** Targets in running text, each in mono: "a", "a and b", "a, b and c" (or "or"). */
const joinTargets = (names, conjunction = 'and') =>
  names.map((name, i) => html`<span key=${`${i}-${name}`}>${i === 0 ? '' : i === names.length - 1 ? ` ${conjunction} ` : ', '}<span class="mono">${name}</span></span>`);

/**
 * When the target an alias tries first fixes the depth, a suffix in the
 * request does nothing there (the pin wins), so the examples would promise
 * what the gateway does not do: say what happens instead.
 */
function PinnedSuffix({ pins }) {
  const later = pins.open;
  const tail =
    later.length > 0
      ? html`<span>${` A suffix only counts when ${later.length === 1 ? 'the later target' : 'one of the later targets'} `}</span>${joinTargets(later, 'or')}<span>${`, which ${later.length === 1 ? 'fixes' : 'fix'} no depth, serves the request instead.`}</span>`
      : html`<span> No target of this alias leaves the depth open, so a suffix never changes how it reasons. To change the depth, edit the alias.</span>`;
  // One line: htm drops the spaces around a line break inside running text.
  return html`
    <p class="models-caption"><span>Requests go to its first target, </span><span class="mono">${pins.first}</span><span>, and that fixes the depth at ${depthPhrase(pins.firstPin)}. A suffix in the request has no effect there.</span>${tail}</p>
  `;
}

function ReasoningSuffix({ row, providers, pins }) {
  if (pins?.firstPin) return html`<${PinnedSuffix} pins=${pins} />`;
  return html`<${SuffixExamples} row=${row} providers=${providers} pinned=${pins?.pinned ?? []} />`;
}

function SuffixExamples({ row, providers, pinned }) {
  // What "automatic" becomes depends on the API the upstream is spoken to
  // in: the families of the protocols its providers take (GET /providers
  // `protocols`). Every family while that is not known.
  const families = useMemo(() => {
    const byName = new Map((providers ?? []).map((p) => [p.name, p]));
    const found = new Set();
    for (const route of row.routes) {
      const protocols = byName.get(route.provider)?.protocols;
      if (!protocols?.length) return undefined;
      for (const protocol of protocols) found.add(protocolFamily(protocol));
    }
    found.delete(null);
    return found.size > 0 ? [...found] : undefined;
  }, [row.routes, providers]);
  const examples = useMemo(() => suffixExamples(row.name, row.info.thinking, families), [row.name, row.info.thinking, families]);
  // A depth pinned on a later alias target beats the suffix when that target serves.
  return html`
    ${row.isAlias &&
    html`<p class="models-caption"><span>The limits below are those of the alias's first target; a request served by another target is fitted to that one.</span>${pinned.length > 0 && html`<span> ${pinned.length === 1 ? 'The target' : 'The targets'} </span>${joinTargets(pinned)}<span>${` ${pinned.length === 1 ? 'fixes its' : 'fix their'} depth in the alias, and that wins over a suffix in the request when ${pinned.length === 1 ? 'it serves' : 'they serve'}.`}</span>`}</p>`}
    <ul class="models-suffixes">
      ${examples.map(
        (example) => html`
          <li key=${example.model} class="models-suffix">
            <div class="models-suffix-top">
              <span class="models-suffix-title">${example.title}</span>
              <span class="mono models-suffix-model">${example.model}</span>
              <${CopyButton} value=${example.model} label=${`Copy ${example.model}`} />
            </div>
            <p class="muted">${example.text}</p>
          </li>
        `,
      )}
    </ul>
  `;
}

/**
 * name          the model in ?open= ('' when closed)
 * row           its table row, undefined when there is none
 * rowsByName    Map(name -> row) of the whole table
 * providers     GET /providers data
 * aliases       GET /aliases data
 * status        GET /status data
 * loading       the model table is on its first load
 * error         the model table could not be loaded at all
 */
export default function ModelDrawer({ name, row, rowsByName, providers, aliases, status, loading, error, onRetry, onClose, onOpen, onEditAliases }) {
  // Keep the last model on screen while the drawer slides out.
  const last = useRef({ name: '', row: undefined });
  if (name) last.current = { name, row };
  const shownName = name || last.current.name;
  const shown = name ? row : last.current.row;

  const pins = useMemo(() => suffixPins(shown, rowsByName), [shown, rowsByName]);

  let body;
  if (shown) {
    body = html`
      <div class="models-detail">
        <div class="models-detail-status">
          <${StatusLamp}
            size="lg"
            tone=${shown.availability.tone}
            label=${shown.availability.label}
            detail=${shown.availability.until ? html`<${BackIn} until=${shown.availability.until} />` : shown.availability.detail}
          />
          <${KindBadges} row=${shown} />
        </div>
        <${AvailabilityNotice} row=${shown} onEditAliases=${onEditAliases} onRetry=${onRetry} />
        <${Section} title="Routes"><${Routes} row=${shown} /><//>
        <${Section} title="Details"><${Details} row=${shown} rowsByName=${rowsByName} aliases=${aliases} onOpen=${onOpen} /><//>
        <${Section} title="Call it" description="The same model answers in every protocol the gateway speaks. Use the name exactly as written.">
          <${CallIt} row=${shown} status=${status} />
        <//>
        ${shown.info.thinking &&
        html`
          <${Section}
            title="Reasoning suffix"
            description=${pins?.firstPin
              ? 'A depth in parentheses after a name sets how hard the model reasons for one request, but this alias sets the depth itself.'
              : 'Add a depth in parentheses to the name to set how hard the model reasons for one request. It wins over reasoning settings in the request body and works in every protocol. An alias target can carry the same suffix.'}
          >
            <${ReasoningSuffix} row=${shown} providers=${providers} pins=${pins} />
          <//>
        `}
      </div>
    `;
  } else if (loading) {
    body = html`<div class="stack"><${Skeleton} width="40%" height="20px" /><${Skeleton} lines=${5} /><${Skeleton} height="120px" /></div>`;
  } else if (error) {
    body = html`<${ErrorState} title="Could not load the model table" error=${error} onRetry=${onRetry} />`;
  } else {
    body = html`
      <${EmptyState}
        icon="models"
        title="No model with this name"
        description="The gateway does not list it. A provider or alias may have been changed or removed since this link was made."
        action=${html`<${Button} onClick=${onClose}>Back to the model table<//>`}
      />
    `;
  }

  return html`
    <${Drawer}
      open=${!!name}
      onClose=${onClose}
      title=${shown ? (shown.isAlias ? 'Alias' : 'Model') : 'Model'}
      subtitle=${shownName}
      width="640px"
      actions=${shownName && html`<${CopyButton} value=${shownName} label="Copy model name" />`}
      footer=${html`
        ${shown?.isAlias && html`<${Button} icon="edit" onClick=${onEditAliases}>Edit aliases<//>`}
        <${Button} data-autofocus="" onClick=${onClose}>Close<//>
      `}
    >
      ${body}
    <//>
  `;
}
