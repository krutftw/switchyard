// TrackDiagram: which providers a model can be routed to, drawn as a yard.
// Models are on the left, providers on the right, and every route is a pair
// of rails. The route a request would take right now is lit; each provider
// ends in its signal lamp.
//
//   html`<${TrackDiagram}
//     models=${[{ id: 'gpt-4o', label: 'gpt-4o' }, { id: 'fast', label: 'fast', note: 'alias' }]}
//     providers=${[
//       { id: 'openai', label: 'openai', state: 'clear', note: '2 credentials' },
//       { id: 'azure-east', label: 'azure-east', state: 'caution', note: 'cooling, 41s' },
//     ]}
//     routes=${[
//       { model: 'gpt-4o', provider: 'openai', order: 1 },
//       { model: 'gpt-4o', provider: 'azure-east', order: 2 },
//       { model: 'fast', provider: 'azure-east', order: 1 },
//     ]}
//   />`

import { html, useState } from '../../vendor/preact-htm.js';
import { cx } from '../lib/dom.js';
import { useSize } from '../lib/hooks.js';
import { StatusLamp } from './status.js';

const ROW = 40;
const STATE_TEXT = { clear: 'serving', caution: 'cooling down', stop: 'failing', info: 'in use', off: 'disabled' };

const roundPx = (v) => Math.round(v * 10) / 10;

/**
 * models     [{ id, label?, note? }]
 * providers  [{ id, label?, note?, state }]  state: a lamp tone
 *            ("clear" serving, "caution" cooling down, "stop" failing, "off" disabled)
 * routes     [{ model, provider, order?, state?, lit? }]
 *            order: failover position, 1 first. state: overrides the
 *            provider's state for this one route (a model-level cooldown).
 *            lit: force which route is shown as set; by default it is the
 *            first route by order whose state is "clear" or "info".
 * selected   id of the model to focus (controlled); omit for hover/click focus
 * onSelect   (id | null) => void
 * label      accessible name of the diagram
 *
 * Below 520px of width the yard is drawn as a list: each model, then its
 * routes in failover order.
 */
export function TrackDiagram({ models = [], providers = [], routes = [], selected, onSelect, label = 'Routes from models to providers', class: className }) {
  const [frame, frameSize] = useSize();
  const [mid, midSize] = useSize();
  const [hovered, setHovered] = useState(null);
  const [picked, setPicked] = useState(null);
  const controlled = selected !== undefined;
  const pinned = controlled ? selected : picked;
  const focus = pinned ?? hovered;

  const providerById = new Map(providers.map((p) => [p.id, p]));
  const modelIndex = new Map(models.map((m, i) => [m.id, i]));
  const providerIndex = new Map(providers.map((p, i) => [p.id, i]));

  // Resolve each route's state and which one is set, per model.
  const resolved = routes
    .filter((r) => modelIndex.has(r.model) && providerIndex.has(r.provider))
    .map((r, i) => ({ ...r, order: r.order ?? i + 1, state: r.state ?? providerById.get(r.provider).state ?? 'off' }));
  const byModel = new Map();
  for (const route of resolved) {
    if (!byModel.has(route.model)) byModel.set(route.model, []);
    byModel.get(route.model).push(route);
  }
  for (const list of byModel.values()) {
    list.sort((a, b) => a.order - b.order);
    if (!list.some((r) => r.lit != null)) {
      const first = list.find((r) => r.state === 'clear' || r.state === 'info');
      if (first) first.lit = true;
    }
  }

  const select = (id) => {
    const next = pinned === id ? null : id;
    if (!controlled) setPicked(next);
    onSelect?.(next);
  };

  const describe = (model) => {
    const list = byModel.get(model.id) ?? [];
    if (list.length === 0) return `${model.label ?? model.id} has no route.`;
    return `${model.label ?? model.id} routes to ${list
      .map((r) => `${providerById.get(r.provider).label ?? r.provider} (${STATE_TEXT[r.state] ?? r.state})`)
      .join(', then ')}.`;
  };

  if (models.length === 0 || providers.length === 0) {
    return html`<div class="chart-empty" style="min-height:120px">No routes to draw yet</div>`;
  }

  // ---- Narrow: a list ---------------------------------------------------
  if (frameSize.width > 0 && frameSize.width < 520) {
    return html`
      <div ref=${frame} class=${cx('track-list', className)} role="group" aria-label=${label}>
        ${models.map((model) => {
          const list = byModel.get(model.id) ?? [];
          return html`
            <div class="track-list-model" key=${model.id}>
              <div class="track-text">
                <span class="track-name" style="color:var(--text)">${model.label ?? model.id}</span>
                ${model.note && html`<span class="track-note">${model.note}</span>`}
              </div>
              ${list.length === 0
                ? html`<span class="faint" style="font-size:var(--text-sm)">No route</span>`
                : html`<ol class="track-list-routes">
                    ${list.map((route) => {
                      const provider = providerById.get(route.provider);
                      return html`
                        <li class="track-list-route" key=${route.provider} data-lit=${route.lit ? '' : undefined}>
                          <${StatusLamp} tone=${route.state} title=${STATE_TEXT[route.state]} />
                          <span class="track-text">
                            <span class="track-name" style=${route.lit ? 'color:var(--text)' : undefined}>${provider.label ?? provider.id}</span>
                            ${provider.note && html`<span class="track-note">${provider.note}</span>`}
                          </span>
                        </li>
                      `;
                    })}
                  </ol>`}
            </div>
          `;
        })}
      </div>
    `;
  }

  // ---- Wide: the yard ---------------------------------------------------
  const rows = Math.max(models.length, providers.length);
  const height = rows * ROW;
  const yOf = (index, count) => (height - count * ROW) / 2 + index * ROW + ROW / 2;
  const width = midSize.width;
  const lead = Math.min(28, width * 0.16);
  const curve = (width - lead * 2) * 0.5;

  const pathFor = (route) => {
    const y1 = yOf(modelIndex.get(route.model), models.length);
    const y2 = yOf(providerIndex.get(route.provider), providers.length);
    return `M0 ${roundPx(y1)}H${roundPx(lead)}C${roundPx(lead + curve)} ${roundPx(y1)} ${roundPx(width - lead - curve)} ${roundPx(y2)} ${roundPx(width - lead)} ${roundPx(y2)}H${roundPx(width)}`;
  };

  // Lit routes are drawn last so they cross over the others.
  const drawOrder = [...resolved].sort((a, b) => Number(!!a.lit) - Number(!!b.lit));
  const focusRoutes = focus ? (byModel.get(focus) ?? []) : null;
  const focusProviders = focusRoutes ? new Map(focusRoutes.map((r) => [r.provider, r])) : null;

  return html`
    <div ref=${frame} class=${cx('track', className)} role="group" aria-label=${label} onPointerLeave=${() => setHovered(null)}>
      <ul class="track-col" data-side="from" style=${`height:${height}px;justify-content:center`}>
        ${models.map(
          (model) => html`
            <li key=${model.id}>
              <button
                type="button"
                class="track-node"
                style="width:100%"
                aria-pressed=${pinned === model.id ? 'true' : 'false'}
                data-lit=${focus === model.id ? '' : undefined}
                data-dim=${focus && focus !== model.id ? '' : undefined}
                onPointerEnter=${(event) => event.pointerType !== 'touch' && setHovered(model.id)}
                onFocus=${() => setHovered(model.id)}
                onBlur=${() => setHovered(null)}
                onClick=${() => select(model.id)}
              >
                <span class="track-text">
                  <span class="track-name">${model.label ?? model.id}</span>
                  ${model.note && html`<span class="track-note">${model.note}</span>`}
                </span>
              </button>
            </li>
          `,
        )}
      </ul>

      <div ref=${mid} class="track-mid" aria-hidden="true">
        ${width > 0 &&
        html`
          <svg class="track-svg" width=${width} height=${height} viewBox=${`0 0 ${width} ${height}`}>
            ${drawOrder.map((route) => {
              const d = pathFor(route);
              return html`
                <g
                  class="track-route"
                  key=${`${route.model}>${route.provider}`}
                  data-lit=${route.lit ? '' : undefined}
                  data-state=${route.state}
                  data-dim=${focus && route.model !== focus ? '' : undefined}
                >
                  <path class="track-rail" d=${d} />
                  <path class="track-bed" d=${d} />
                </g>
              `;
            })}
          </svg>
        `}
      </div>

      <ul class="track-col" data-side="to" style=${`height:${height}px;justify-content:center`}>
        ${providers.map((provider) => {
          const viaFocus = focusProviders?.get(provider.id);
          const state = viaFocus?.state ?? provider.state ?? 'off';
          return html`
            <li key=${provider.id} class="track-node" data-lit=${viaFocus ? '' : undefined} data-dim=${focusProviders && !viaFocus ? '' : undefined}>
              <${StatusLamp} tone=${state} title=${STATE_TEXT[state] ?? state} />
              <span class="track-text">
                <span class="track-name">${viaFocus && focusRoutes.length > 1 ? `${viaFocus.order}. ` : ''}${provider.label ?? provider.id}</span>
                ${provider.note && html`<span class="track-note">${provider.note}</span>`}
              </span>
            </li>
          `;
        })}
      </ul>

      <ul class="sr-only">
        ${models.map((model) => html`<li key=${model.id}>${describe(model)}</li>`)}
      </ul>
    </div>
  `;
}
