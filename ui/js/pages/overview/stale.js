// Overview: saying that something on screen is old. A refetch that fails
// leaves the last data in place; each section then says since when, and one
// notice at the top says why and offers the retry.

import { html } from '../../../vendor/preact-htm.js';
import { StatusLamp } from '../../components/index.js';
import { formatTime } from '../../lib/format.js';

/** True when a useResource shows data its last refetch could not renew. */
export const isStale = (resource) => Boolean(resource?.error) && resource.data != null;

/**
 * "Not refreshed since 14:03:27" after a panel's description, when one of
 * `resources` is stale. Renders nothing otherwise.
 */
export function StaleMark({ resources }) {
  const stale = resources.filter(isStale);
  if (stale.length === 0) return null;
  const since = Math.min(...stale.map((resource) => resource.updatedAt ?? Infinity));
  return html`<span class="overview-stale"><${StatusLamp} tone="caution" label=${Number.isFinite(since) ? `Not refreshed since ${formatTime(since)}` : 'Not refreshed'} /></span>`;
}

/** A panel description with the stale mark after it. */
export function described(text, ...resources) {
  return html`<span class="overview-desc-text">${text}</span><${StaleMark} resources=${resources} />`;
}

/**
 * Move the keyboard focus after the next render, for a control that removes
 * itself when used (a dismiss button, "Show again"). `pick` returns the
 * element to focus, or nothing. A timeout, not an animation frame: frames do
 * not run in a background tab.
 */
export function focusSoon(pick) {
  setTimeout(() => pick()?.focus?.(), 0);
}

/** The first element carrying data-overview-focus=`name`, optionally the one at `index`. */
export function focusTarget(name, index = 0) {
  if (typeof document === 'undefined') return null;
  const all = document.querySelectorAll(`[data-overview-focus="${name}"]`);
  return all[Math.min(index, all.length - 1)] ?? null;
}

export default StaleMark;
