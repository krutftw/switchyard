// Icons: one small inline SVG set, 24x24 grid, 1.75 stroke, round caps.
//
//   import { Icon } from '../components/icons.js';
//   html`<${Icon} name="search" />`               // 16px, decorative
//   html`<${Icon} name="alert" size=${20} label="Warning" />`
//
// Icons inherit `currentColor`. They are decorative (aria-hidden) unless a
// `label` is given. Do not use emoji or Unicode symbols as icons; add a path
// here instead.
//
// The railway glyphs (switch, signal, junction, buffer) are drawn for
// Switchyard. The general-purpose glyphs are adapted from Lucide
// (https://lucide.dev, ISC licence; see ui/vendor/LICENSES.txt).

import { html } from './vendor/preact-htm.js';

/** name -> SVG inner markup (strings of path data keep the set compact). */
const PATHS = {
  // ---- Switchyard glyphs -------------------------------------------------
  // The mark: a straight track with a diverging route and its lamp.
  switch: html`<path d="M2 16.5h20" /><path d="M2 16.5h5.5c4.5 0 4.5-9 9-9H22" /><circle cx="19" cy="7.5" r="0.5" fill="currentColor" />`,
  signal: html`<rect x="8" y="2" width="8" height="13" rx="4" /><circle cx="12" cy="6.25" r="1.25" /><circle cx="12" cy="10.75" r="1.25" fill="currentColor" /><path d="M12 15v7M8.5 22h7" />`,
  junction: html`<path d="M3 12h18" /><path d="M3 12h4c3.5 0 3.5-6 7-6h7" /><path d="M3 12h4c3.5 0 3.5 6 7 6h7" />`,
  buffer: html`<path d="M3 12h14M17 7v10M21 7v10M17 12h4" />`,

  // ---- Navigation --------------------------------------------------------
  overview: html`<path d="M3.3 18.5a10 10 0 1 1 17.4 0" /><path d="m12 13 4.5-4.5" /><circle cx="12" cy="13" r="1.25" />`,
  requests: html`<path d="m17 3 4 4-4 4" /><path d="M21 7H7" /><path d="m7 21-4-4 4-4" /><path d="M3 17h14" />`,
  providers: html`<rect x="2.5" y="3.5" width="19" height="7" rx="1.5" /><rect x="2.5" y="13.5" width="19" height="7" rx="1.5" /><path d="M6.5 7h.01M6.5 17h.01M10.5 7h4M10.5 17h4" />`,
  models: html`<path d="M21 8 12 3 3 8v8l9 5 9-5Z" /><path d="m3 8 9 5 9-5" /><path d="M12 13v8" />`,
  key: html`<circle cx="7.5" cy="15.5" r="4.5" /><path d="m10.8 12.2 9.7-9.7" /><path d="m16.5 6.5 3 3" /><path d="m13.5 9.5 2 2" />`,
  usage: html`<path d="M3 3v18h18" /><path d="M8 17v-4" /><path d="M13 17V8" /><path d="M18 17v-7" />`,
  playground: html`<path d="m4 17 6-5-6-5" /><path d="M12 19h8" />`,
  logs: html`<path d="M4 6h16" /><path d="M4 10h10" /><path d="M4 14h16" /><path d="M4 18h7" />`,
  settings: html`<path d="M3 6h10M19 6h2" /><circle cx="16" cy="6" r="2.25" /><path d="M3 12h2M11 12h10" /><circle cx="8" cy="12" r="2.25" /><path d="M3 18h9M18 18h3" /><circle cx="15" cy="18" r="2.25" />`,
  about: html`<circle cx="12" cy="12" r="9.5" /><path d="M12 16.5v-5" /><path d="M12 7.75h.01" />`,
  kit: html`<rect x="3" y="3" width="7.5" height="7.5" rx="1.5" /><rect x="13.5" y="3" width="7.5" height="7.5" rx="1.5" /><rect x="3" y="13.5" width="7.5" height="7.5" rx="1.5" /><rect x="13.5" y="13.5" width="7.5" height="7.5" rx="1.5" />`,

  // ---- Actions -----------------------------------------------------------
  search: html`<circle cx="11" cy="11" r="7" /><path d="m20.5 20.5-4.5-4.5" />`,
  x: html`<path d="M18 6 6 18" /><path d="m6 6 12 12" />`,
  check: html`<path d="M20 6 9 17l-5-5" />`,
  plus: html`<path d="M12 5v14" /><path d="M5 12h14" />`,
  minus: html`<path d="M5 12h14" />`,
  copy: html`<rect x="9" y="9" width="12" height="12" rx="2" /><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />`,
  eye: html`<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12Z" /><circle cx="12" cy="12" r="3" />`,
  'eye-off': html`<path d="M10 5.2A9.6 9.6 0 0 1 12 5c6.5 0 10 7 10 7a15 15 0 0 1-2.2 3.1" /><path d="M6.4 6.5C3.6 8.3 2 12 2 12s3.5 7 10 7a9.8 9.8 0 0 0 5.5-1.6" /><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2" /><path d="m3 3 18 18" />`,
  trash: html`<path d="M3 6h18" /><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /><path d="m19 6-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" /><path d="M10 11v6M14 11v6" />`,
  edit: html`<path d="M12 20h9" /><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" />`,
  refresh: html`<path d="M21 12a9 9 0 1 1-2.6-6.4L21 8" /><path d="M21 3v5h-5" />`,
  external: html`<path d="M15 3h6v6" /><path d="M10 14 21 3" /><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />`,
  download: html`<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><path d="m7 10 5 5 5-5" /><path d="M12 15V3" />`,
  filter: html`<path d="M3 5h18" /><path d="M6.5 12h11" /><path d="M10 19h4" />`,
  play: html`<path d="M7 4.5v15l12-7.5Z" />`,
  pause: html`<path d="M8 5v14M16 5v14" />`,
  stop: html`<rect x="6" y="6" width="12" height="12" rx="1.5" />`,
  // Six dots: the handle of a row that can be dragged to reorder.
  grip: html`<circle cx="9" cy="6" r="1.25" fill="currentColor" /><circle cx="15" cy="6" r="1.25" fill="currentColor" /><circle cx="9" cy="12" r="1.25" fill="currentColor" /><circle cx="15" cy="12" r="1.25" fill="currentColor" /><circle cx="9" cy="18" r="1.25" fill="currentColor" /><circle cx="15" cy="18" r="1.25" fill="currentColor" />`,
  logout: html`<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><path d="m16 17 5-5-5-5" /><path d="M21 12H9" />`,
  wrap: html`<path d="M3 6h18" /><path d="M3 12h15a3 3 0 1 1 0 6h-4" /><path d="m16 16-2 2 2 2" /><path d="M3 18h7" />`,
  table: html`<rect x="3" y="3" width="18" height="18" rx="2" /><path d="M3 9h18M3 15h18M9 3v18" />`,
  chart: html`<path d="M3 3v18h18" /><path d="m7 15 4-5 4 3 5-7" />`,
  send: html`<path d="M21 3 10.5 13.5" /><path d="M21 3 14.5 21l-4-7.5-7.500-4Z" />`,
  zap: html`<path d="M13 2 4 14h7l-1 8 9-12h-7Z" />`,
  link: html`<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.700 1.700" /><path d="M14 11a5 5 0 0 0-7.500-.5l-3 3a5 5 0 0 0 7 7l1.700-1.700" />`,

  // ---- Chrome ------------------------------------------------------------
  menu: html`<path d="M4 6h16" /><path d="M4 12h16" /><path d="M4 18h16" />`,
  more: html`<circle cx="5" cy="12" r="1.25" fill="currentColor" /><circle cx="12" cy="12" r="1.25" fill="currentColor" /><circle cx="19" cy="12" r="1.25" fill="currentColor" />`,
  'chevron-down': html`<path d="m6 9 6 6 6-6" />`,
  'chevron-up': html`<path d="m18 15-6-6-6 6" />`,
  'chevron-left': html`<path d="m15 18-6-6 6-6" />`,
  'chevron-right': html`<path d="m9 18 6-6-6-6" />`,
  sort: html`<path d="m7 15 5 5 5-5" /><path d="m7 9 5-5 5 5" />`,
  'arrow-up': html`<path d="M12 19V5" /><path d="m5 12 7-7 7 7" />`,
  'arrow-down': html`<path d="M12 5v14" /><path d="m19 12-7 7-7-7" />`,
  'arrow-right': html`<path d="M5 12h14" /><path d="m12 5 7 7-7 7" />`,
  'arrow-left': html`<path d="M19 12H5" /><path d="m12 19-7-7 7-7" />`,
  sidebar: html`<rect x="3" y="3" width="18" height="18" rx="2" /><path d="M9 3v18" />`,
  command: html`<path d="M15 6v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3" />`,
  sun: html`<circle cx="12" cy="12" r="4" /><path d="M12 2v2M12 20v2M4.900 4.900l1.400 1.400M17.700 17.700l1.400 1.400M2 12h2M20 12h2M4.900 19.100l1.400-1.400M17.700 6.300l1.400-1.400" />`,
  moon: html`<path d="M21 12.800A9 9 0 1 1 11.200 3a7 7 0 0 0 9.800 9.800Z" />`,
  monitor: html`<rect x="2" y="3" width="20" height="14" rx="2" /><path d="M8 21h8" /><path d="M12 17v4" />`,
  clock: html`<circle cx="12" cy="12" r="9.5" /><path d="M12 6.500V12l3.500 2" />`,

  // ---- Status ------------------------------------------------------------
  alert: html`<path d="M10.300 3.900 1.800 18a2 2 0 0 0 1.700 3h17a2 2 0 0 0 1.700-3L13.700 3.900a2 2 0 0 0-3.400 0Z" /><path d="M12 9v4" /><path d="M12 17h.01" />`,
  'alert-circle': html`<circle cx="12" cy="12" r="9.5" /><path d="M12 7.500v5" /><path d="M12 16.250h.01" />`,
  info: html`<circle cx="12" cy="12" r="9.5" /><path d="M12 16.500v-5" /><path d="M12 7.750h.01" />`,
  'check-circle': html`<circle cx="12" cy="12" r="9.500" /><path d="m8.500 12.500 2.500 2.500 4.500-5" />`,
  'x-circle': html`<circle cx="12" cy="12" r="9.500" /><path d="m15 9-6 6" /><path d="m9 9 6 6" />`,
  lock: html`<rect x="4" y="11" width="16" height="10" rx="2" /><path d="M8 11V7a4 4 0 0 1 8 0v4" />`,
  plug: html`<path d="M12 22v-5" /><path d="M9 8V2" /><path d="M15 8V2" /><path d="M18 8v4a6 6 0 0 1-12 0V8Z" />`,
};

/** Every icon name, for the kitchen sink. */
export const ICON_NAMES = Object.keys(PATHS);

/**
 * @param {{ name: string, size?: number, label?: string, class?: string, strokeWidth?: number }} props
 */
export function Icon({ name, size = 16, label, class: className, strokeWidth = 1.75 }) {
  const body = PATHS[name];
  if (!body) {
    // A missing icon is a bug worth seeing in development, not a crash.
    console.warn(`Unknown icon "${name}"`);
    return null;
  }
  return html`
    <svg
      class=${className ? `icon ${className}` : 'icon'}
      width=${size}
      height=${size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width=${strokeWidth}
      stroke-linecap="round"
      stroke-linejoin="round"
      role=${label ? 'img' : undefined}
      aria-label=${label || undefined}
      aria-hidden=${label ? undefined : 'true'}
      focusable="false"
    >
      ${body}
    </svg>
  `;
}

/**
 * The Switchyard mark: a track switch with the diverging route lit.
 * `lit` colours the diverging route with the accent. `draw` plays the route
 * being set once (sign-in page only).
 */
export function LogoMark({ size = 24, lit = true, draw = false, class: className }) {
  return html`
    <svg
      class=${className ? `logo-mark ${className}` : 'logo-mark'}
      data-draw=${draw ? '' : undefined}
      width=${size}
      height=${size}
      viewBox="0 0 24 24"
      fill="none"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M2 16.5h20" stroke="currentColor" stroke-width="2" opacity="0.45" />
      <path
        class="logo-route"
        d="M2 16.5h5.500c4.500 0 4.500-9 9-9H19"
        stroke=${lit ? 'var(--accent)' : 'currentColor'}
        stroke-width="2"
      />
      <circle class="logo-lamp" cx="20.500" cy="7.500" r="1.900" fill=${lit ? 'var(--accent)' : 'currentColor'} />
    </svg>
  `;
}
