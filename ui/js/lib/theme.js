// Theme: follows the operating system until the user picks one, then stays
// on the pick (persisted in localStorage). The inline script in index.html
// applies the same rule before first paint so there is no flash.
//
//   import { theme, setThemePref } from '../lib/theme.js';
//   const { pref, resolved } = useStore(theme);   // pref: system | light | dark
//   setThemePref('light');

import { createStore } from './store.js';

export const THEME_KEY = 'sy.theme';

const THEME_COLOR = { dark: '#101316', light: '#eff1f4' };

const media = typeof matchMedia === 'undefined' ? null : matchMedia('(prefers-color-scheme: light)');

function readPref() {
  try {
    const v = localStorage.getItem(THEME_KEY);
    return v === 'light' || v === 'dark' ? v : 'system';
  } catch {
    return 'system';
  }
}

const resolve = (pref) => (pref === 'system' ? (media?.matches ? 'light' : 'dark') : pref);

const initialPref = readPref();

export const theme = createStore({ pref: initialPref, resolved: resolve(initialPref) });

function apply() {
  const { resolved } = theme.get();
  if (typeof document === 'undefined') return;
  document.documentElement.dataset.theme = resolved;
  document.querySelector('meta[name="theme-color"]')?.setAttribute('content', THEME_COLOR[resolved]);
}

/** @param {"system"|"light"|"dark"} pref */
export function setThemePref(pref) {
  try {
    if (pref === 'system') localStorage.removeItem(THEME_KEY);
    else localStorage.setItem(THEME_KEY, pref);
  } catch {
    /* storage unavailable: the choice lasts for this page load */
  }
  theme.set({ pref, resolved: resolve(pref) });
  apply();
}

media?.addEventListener('change', () => {
  const { pref } = theme.get();
  if (pref !== 'system') return;
  theme.set({ resolved: resolve('system') });
  apply();
});

apply();
