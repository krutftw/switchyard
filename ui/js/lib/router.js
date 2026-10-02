// Hash router. The dashboard is a static bundle served at /admin/, so routes
// live in the fragment ("#/requests/req_123?status=error") and need no server
// rewrites.
//
//   import { useRoute, navigate, href, useQueryParam } from '../lib/router.js';
//   const route = useRoute();                 // { path, segments, query }
//   navigate('/providers');                   // push
//   navigate('/requests', { query: { status: 'error' }, replace: true });
//   html`<a href=${href('/requests/' + id)}>Open</a>`
//   const [range, setRange] = useQueryParam('range', '24h');
//
// Keep filters, tabs and the selected entity in the query so a link reproduces
// the view.

import { useCallback } from '../../vendor/preact-htm.js';
import { createStore, useStore } from './store.js';

/** "#/a/b?x=1" -> { path: "/a/b", segments: ["a","b"], query: { x: "1" } } */
export function parseHash(hash) {
  let raw = (hash || '').replace(/^#/, '');
  if (!raw.startsWith('/')) raw = `/${raw}`;
  const q = raw.indexOf('?');
  const pathPart = q === -1 ? raw : raw.slice(0, q);
  const query = {};
  if (q !== -1) {
    for (const [key, value] of new URLSearchParams(raw.slice(q + 1))) query[key] = value;
  }
  const segments = pathPart
    .split('/')
    .filter(Boolean)
    .map((s) => {
      try {
        return decodeURIComponent(s);
      } catch {
        return s;
      }
    });
  return { path: `/${segments.join('/')}`, segments, query };
}

/** Build a fragment URL for a path and optional query object. */
export function href(path, query) {
  const clean = `/${String(path).split('/').filter(Boolean).map(encodeURIComponent).join('/')}`;
  const params = new URLSearchParams();
  if (query) {
    for (const [key, value] of Object.entries(query)) {
      if (value == null || value === '' || value === false) continue;
      params.set(key, String(value));
    }
  }
  const qs = params.toString();
  return `#${clean}${qs ? `?${qs}` : ''}`;
}

const current = () => parseHash(typeof location === 'undefined' ? '' : location.hash);

export const routeStore = createStore(current());

if (typeof window !== 'undefined') {
  window.addEventListener('hashchange', () => routeStore.replace(current()));
}

/**
 * Go to a route. `replace` swaps the current history entry instead of adding
 * one; use it for filter changes so Back leaves the page.
 */
export function navigate(path, { query, replace = false } = {}) {
  const target = href(path, query);
  if (target === location.hash) return;
  if (replace) {
    const url = new URL(location.href);
    url.hash = target;
    history.replaceState(history.state, '', url);
    routeStore.replace(current());
  } else {
    location.hash = target;
  }
}

/** Merge `patch` into the current query. null, "" and false remove a key. */
export function setQuery(patch, { replace = true } = {}) {
  const route = routeStore.get();
  navigate(route.path, { query: { ...route.query, ...patch }, replace });
}

export function useRoute() {
  return useStore(routeStore);
}

/**
 * One query parameter as state. Reading returns `fallback` when the key is
 * absent; writing the fallback removes the key so default views keep clean
 * URLs.
 */
export function useQueryParam(key, fallback = '') {
  const value = useStore(routeStore, (r) => r.query[key]);
  const set = useCallback(
    (next) => setQuery({ [key]: next === fallback ? null : next }),
    [key, fallback],
  );
  return [value ?? fallback, set];
}
