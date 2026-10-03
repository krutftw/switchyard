// The signed-in frame: sidebar (a bottom bar and sheet on phones), top bar,
// the routed page, and the overlays every page shares (toasts, confirm
// dialog, command palette).

import { html, useEffect, useMemo, useRef, useState, useErrorBoundary } from '../../vendor/preact-htm.js';
import { Button, IconButton } from '../components/button.js';
import { Icon, LogoMark } from '../components/icons.js';
import { Menu } from '../components/menu.js';
import { ConfirmHost, Drawer } from '../components/overlay.js';
import { Kbd, StatusLamp } from '../components/status.js';
import { EmptyState, ErrorState, Page, Skeleton } from '../components/surface.js';
import { Toaster } from '../components/toast.js';
import { Tooltip } from '../components/tooltip.js';
import { api } from '../lib/api.js';
import { overlayLocked } from '../lib/dom.js';
import { formatTime } from '../lib/format.js';
import { hotkeyLabel, useHotkey, useIsPhone, useLocalStorage, useNow, useResource } from '../lib/hooks.js';
import { live, liveState } from '../lib/live.js';
import { href, mayLeave, navigate, useRoute } from '../lib/router.js';
import { useStore } from '../lib/store.js';
import { setThemePref, theme } from '../lib/theme.js';
import { DEFAULT_ROUTE, NAV_GROUPS, ROUTES, matchRoute } from '../routes.js';
import { CommandPalette } from './palette.js';

/**
 * Sign out, unless a view has something to lose and the user decides to
 * stay (lib/router.js, leave guards). Every sign-out control goes through
 * here. A session the gateway ends (a 401) cannot be held back.
 */
export async function signOut() {
  if (await mayLeave(null, 'signout')) api.logout();
}

// ---------------------------------------------------------------------------
// Live connection indicator
// ---------------------------------------------------------------------------

const LIVE_VIEW = {
  idle: { tone: 'off', label: 'Not connected' },
  connecting: { tone: 'info', label: 'Connecting', pulse: true },
  open: { tone: 'clear', label: 'Live', pulse: true },
  reconnecting: { tone: 'caution', label: 'Reconnecting' },
  offline: { tone: 'stop', label: 'Offline' },
  unavailable: { tone: 'off', label: 'Live updates off' },
};

function liveDetail(state, now) {
  switch (state.status) {
    case 'open':
      return `Live updates are on. Connected since ${formatTime(state.since)}.`;
    case 'connecting':
      return 'Opening the live connection to the gateway.';
    case 'reconnecting':
    case 'offline': {
      const wait = state.retryAt ? Math.max(0, Math.ceil((state.retryAt - now) / 1000)) : 0;
      const lead = state.status === 'offline' ? 'The gateway is not answering. Pages show the last data they loaded.' : 'The live connection dropped.';
      return `${lead} Next attempt ${wait > 0 ? `in ${wait}s` : 'now'}. Select to retry at once.`;
    }
    case 'unavailable':
      return 'The gateway refused the live connection. Pages still load; they will not update on their own.';
    default:
      return 'Live updates start after sign-in.';
  }
}

function LiveChip() {
  const state = useStore(liveState);
  const waiting = state.status === 'reconnecting' || state.status === 'offline';
  // Tick only while there is a countdown to show.
  const now = useNow(waiting ? 1000 : 60_000);
  const view = LIVE_VIEW[state.status] ?? LIVE_VIEW.idle;
  return html`
    <${Tooltip} content=${liveDetail(state, now)} side="bottom" align="end">
      <button type="button" class="live-chip" aria-label=${`Connection: ${view.label}`} onClick=${() => waiting && live.reconnectNow()}>
        <${StatusLamp} tone=${view.tone} pulse=${view.pulse} title=${view.label} />
        <span class="live-chip-label">${view.label}</span>
      </button>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

const THEME_OPTIONS = [
  { pref: 'system', label: 'Match system', icon: 'monitor' },
  { pref: 'light', label: 'Light', icon: 'sun' },
  { pref: 'dark', label: 'Dark', icon: 'moon' },
];

/** The theme menu. Exported for the sign-in page. */
export function ThemeMenu({ size = 'md' }) {
  const { pref, resolved } = useStore(theme);
  return html`
    <${Menu}
      label="Theme"
      icon=${resolved === 'light' ? 'sun' : 'moon'}
      size=${size}
      items=${THEME_OPTIONS.map((option) => ({
        label: option.label,
        icon: option.icon,
        checked: pref === option.pref,
        onSelect: () => setThemePref(option.pref),
      }))}
    />
  `;
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

function NavLink({ entry, current, rail, onNavigate }) {
  const link = html`
    <a class="nav-item" href=${href(entry.path)} aria-current=${current ? 'page' : undefined} aria-label=${rail ? entry.title : undefined} onClick=${onNavigate}>
      <${Icon} name=${entry.icon} size=${18} />
      <span class="nav-item-label">${entry.title}</span>
    </a>
  `;
  return rail ? html`<${Tooltip} content=${entry.title} side="right" delay=${200} describe=${false}>${link}<//>` : link;
}

function NavGroups({ currentPath, rail = false, onNavigate }) {
  return NAV_GROUPS.map((group) => {
    const entries = ROUTES.filter((entry) => entry.group === group);
    if (entries.length === 0) return null;
    return html`
      <div class="nav-group" key=${group} role="group" aria-label=${group}>
        <div class="nav-group-label" aria-hidden="true">${group}</div>
        ${entries.map((entry) => html`<${NavLink} key=${entry.path} entry=${entry} current=${entry.path === currentPath} rail=${rail} onNavigate=${onNavigate} />`)}
      </div>
    `;
  });
}

function Sidebar({ currentPath, rail, onToggleRail, version }) {
  return html`
    <aside class="sidebar">
      <a class="brand" href=${href(DEFAULT_ROUTE)} aria-label="Switchyard, overview">
        <${LogoMark} size=${24} />
        <span class="brand-name">Switchyard</span>
      </a>
      <nav class="nav" aria-label="Main">
        <${NavGroups} currentPath=${currentPath} rail=${rail} />
      </nav>
      <div class="sidebar-foot">
        <span class="sidebar-version" title="Gateway version">${version ? `v${String(version).replace(/^v/, '')}` : ''}</span>
        <${IconButton}
          icon="sidebar"
          label=${`${rail ? 'Expand' : 'Collapse'} sidebar (${hotkeyLabel('mod+b')})`}
          size="sm"
          tooltipSide="right"
          aria-pressed=${rail ? 'true' : 'false'}
          onClick=${onToggleRail}
        />
      </div>
    </aside>
  `;
}

function BottomBar({ currentPath, moreOpen, onMore }) {
  const primary = ROUTES.filter((entry) => entry.primary).slice(0, 4);
  const inMore = !primary.some((entry) => entry.path === currentPath);
  return html`
    <nav class="bottombar" aria-label="Main">
      ${primary.map(
        (entry) => html`
          <a key=${entry.path} class="bottombar-item" href=${href(entry.path)} aria-current=${entry.path === currentPath ? 'page' : undefined}>
            <${Icon} name=${entry.icon} size=${20} />
            <span>${entry.title}</span>
          </a>
        `,
      )}
      <button type="button" class="bottombar-item" aria-haspopup="dialog" aria-expanded=${moreOpen ? 'true' : 'false'} aria-current=${inMore ? 'page' : undefined} onClick=${onMore}>
        <${Icon} name="menu" size=${20} />
        <span>More</span>
      </button>
    </nav>
  `;
}

function MoreSheet({ open, onClose, currentPath, version }) {
  const { pref } = useStore(theme);
  return html`
    <${Drawer} open=${open} onClose=${onClose} side="bottom" title="Switchyard" subtitle=${version ? `v${String(version).replace(/^v/, '')}` : undefined}>
      <nav class="sheet-nav" aria-label="All pages">
        ${ROUTES.filter((entry) => entry.group).map(
          (entry) => html`
            <a key=${entry.path} class="nav-item" href=${href(entry.path)} aria-current=${entry.path === currentPath ? 'page' : undefined} onClick=${onClose}>
              <${Icon} name=${entry.icon} size=${18} />
              <span class="nav-item-label">${entry.title}</span>
            </a>
          `,
        )}
      </nav>
      <div class="sheet-section">
        <div class="plate-label">Theme</div>
        <div class="btn-group">
          ${THEME_OPTIONS.map(
            (option) => html`
              <${Button} key=${option.pref} icon=${option.icon} aria-pressed=${pref === option.pref ? 'true' : 'false'} variant=${pref === option.pref ? 'secondary' : 'ghost'} onClick=${() => setThemePref(option.pref)}>
                ${option.label}
              <//>
            `,
          )}
        </div>
      </div>
      <div class="sheet-section">
        <${Button} icon="logout" block onClick=${signOut}>Sign out<//>
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// The routed page
// ---------------------------------------------------------------------------

function PageSkeleton() {
  return html`
    <div class="page-loading" aria-busy="true" aria-label="Loading page">
      <${Skeleton} width="180px" height="26px" />
      <${Skeleton} width="min(420px, 80%)" height="14px" />
      <div class="panel"><div class="panel-body"><${Skeleton} lines=${5} /></div></div>
    </div>
  `;
}

/** Catches render errors in a page so the shell stays usable. */
function PageBoundary({ children }) {
  const [error, reset] = useErrorBoundary((cause) => console.error('Page crashed', cause));
  if (error) {
    return html`
      <${Page} title="Something went wrong">
        <div class="panel">
          <${ErrorState}
            title="This page hit an error"
            description="The rest of the dashboard still works. Try the page again; if it keeps failing, the browser console has the details."
            error=${null}
            onRetry=${reset}
          />
          <p class="empty-detail" style="padding:0 var(--space-4) var(--space-5);text-align:center">${String(error?.message ?? error)}</p>
        </div>
      <//>
    `;
  }
  return children;
}

function PageHost({ entry, route }) {
  const [loaded, setLoaded] = useState({ path: null, Component: null, error: null });

  useEffect(() => {
    let alive = true;
    entry
      .load()
      .then((module) => {
        if (!alive) return;
        if (typeof module.default !== 'function') throw new Error(`${entry.path} has no default export`);
        setLoaded({ path: entry.path, Component: module.default, error: null });
      })
      .catch((error) => {
        console.error(`Could not load page ${entry.path}`, error);
        if (alive) setLoaded({ path: entry.path, Component: null, error });
      });
    return () => {
      alive = false;
    };
  }, [entry]);

  if (loaded.path !== entry.path) return html`<${PageSkeleton} />`;
  if (loaded.error) {
    return html`
      <${Page} title=${entry.title}>
        <div class="panel">
          <${EmptyState}
            icon="alert"
            title="Could not load this page"
            description="A file the page needs did not arrive. Check the connection to the gateway and reload."
            action=${html`<${Button} icon="refresh" onClick=${() => location.reload()}>Reload<//>`}
          />
        </div>
      <//>
    `;
  }
  const Component = loaded.Component;
  return html`<${Component} route=${route} />`;
}

function NotFound({ route }) {
  return html`
    <${Page} title="Page not found">
      <div class="panel">
        <${EmptyState}
          icon="junction"
          title="No page at this address"
          description=${html`Nothing is routed to <span class="mono">#${route.path}</span>. Pick a page from the navigation, or go back to the overview.`}
          action=${html`<${Button} variant="primary" href=${href(DEFAULT_ROUTE)}>Go to overview<//>`}
        />
      </div>
    <//>
  `;
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

/** How long after a change of page the skip link refuses a focus it was not tabbed to. */
const SKIP_GUARD_MS = 1500;

/**
 * takeFocus  the shell replaces something the user was working in (the
 *            sign-in form, the "Try again" of the boot screen), whose focused
 *            control is gone with it: the focus goes to the page's main
 *            region, as after in-app navigation, instead of being left on
 *            <body>. Not on a plain load, where the first Tab should still
 *            reach "Skip to content".
 */
export function Shell({ takeFocus = false } = {}) {
  const route = useRoute();
  const isPhone = useIsPhone();
  const [rail, setRail] = useLocalStorage('nav.rail', false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [moreOpen, setMoreOpen] = useState(false);
  const main = useRef(null);

  const status = useResource('/status');
  const hello = useStore(liveState, (s) => s.hello);
  const version = hello?.version ?? status.data?.version ?? null;

  // "#/" and "" land on the default page without adding a history entry.
  useEffect(() => {
    if (route.path === '/') navigate(DEFAULT_ROUTE, { replace: true });
  }, [route.path]);

  const entry = matchRoute(route);
  const currentPath = entry?.path ?? null;

  // A new page starts at the top, with focus on the content for screen
  // readers and keyboard users. Query changes (filters, tabs) leave both alone.
  const firstPage = useRef(true);
  const routedAt = useRef(0);
  useEffect(() => {
    if (firstPage.current) {
      firstPage.current = false;
      // Unless something has taken the focus meanwhile (a page that focuses
      // its own field), it was dropped with the sign-in form.
      const at = document.activeElement;
      if (takeFocus && (!at || at === document.body || !document.contains(at))) main.current?.focus({ preventScroll: true });
      return;
    }
    routedAt.current = Date.now();
    window.scrollTo(0, 0);
    // A page that has put the focus somewhere of its own (its search
    // field) keeps it; anywhere else (the link that was followed, <body>)
    // it goes to the page's main region.
    const at = document.activeElement;
    const placed = at && at !== main.current && main.current?.contains(at);
    if (!placed) main.current?.focus({ preventScroll: true });
    setMoreOpen(false);
  }, [currentPath]);

  // "Skip to content" is for the first Tab of a page load. Right after a
  // change of page nothing else may leave the focus on it, where it shows
  // over the top bar (a phone's browser handing the focus back to the top
  // of the document after the fragment changed): the page's main region is
  // where the focus belongs then. Tab still reaches it as usual.
  const tabbedAt = useRef(0);
  useEffect(() => {
    const onKey = (event) => {
      if (event.key === 'Tab') tabbedAt.current = Date.now();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, []);
  const onSkipFocus = () => {
    const now = Date.now();
    if (now - routedAt.current < SKIP_GUARD_MS && now - tabbedAt.current > 1000) main.current?.focus({ preventScroll: true });
  };

  // Ctrl/Cmd+K toggles the palette. It may open over a drawer or a dialog
  // (its commands go through the leave guards like any other navigation),
  // but not while a layer that is not dismissable is open: that layer has
  // said it must be dealt with first (a save in flight, a secret shown
  // once). Every layer is asked, not only the topmost: a menu open inside
  // such a dialog is itself dismissable and does not change the answer.
  // `inLayer`: the key is pressed with focus inside whatever layer is open,
  // the palette itself included.
  const openPalette = () => {
    if (overlayLocked()) return;
    setPaletteOpen(true);
  };
  useHotkey(
    'mod+k',
    () => {
      if (paletteOpen) setPaletteOpen(false);
      else openPalette();
    },
    { inLayer: true },
  );
  useHotkey('mod+b', () => setRail((value) => !value), { enabled: !isPhone });

  const commands = useMemo(
    () => [
      // Pages for page authors (the component kit) are opened by address.
      ...ROUTES.filter((item) => !item.dev).map((item) => ({
        id: `page:${item.path}`,
        label: item.title,
        group: 'Pages',
        icon: item.icon,
        keywords: item.keywords,
        run: () => navigate(item.path),
      })),
      ...THEME_OPTIONS.map((option) => ({
        id: `theme:${option.pref}`,
        label: option.pref === 'system' ? 'Theme: match system' : `Theme: ${option.label.toLowerCase()}`,
        group: 'Actions',
        icon: option.icon,
        keywords: 'theme appearance colour color mode',
        run: () => setThemePref(option.pref),
      })),
      {
        id: 'action:reconnect',
        label: 'Reconnect live updates',
        group: 'Actions',
        icon: 'plug',
        keywords: 'websocket live offline retry',
        run: () => live.reconnectNow(),
      },
      {
        id: 'action:signout',
        label: 'Sign out',
        group: 'Actions',
        icon: 'logout',
        keywords: 'log out logout leave lock',
        run: () => signOut(),
      },
    ],
    [],
  );

  return html`
    <a class="skip-link" href="#main" onFocus=${onSkipFocus} onClick=${(event) => {
      // "#main" would be read as a route: move focus by hand instead.
      event.preventDefault();
      main.current?.focus();
    }}>Skip to content</a>
    <div class="shell" data-rail=${rail && !isPhone ? '' : undefined}>
      <${Sidebar} currentPath=${currentPath} rail=${rail && !isPhone} onToggleRail=${() => setRail(!rail)} version=${version} />
      <div class="shell-main">
        <header class="topbar">
          <a class="topbar-brand" href=${href(DEFAULT_ROUTE)} aria-label="Switchyard, overview">
            <${LogoMark} size=${22} />
            <span class="brand-name">Switchyard</span>
          </a>
          <button type="button" class="palette-trigger" aria-label="Open the command palette" aria-keyshortcuts="Control+K Meta+K" onClick=${openPalette}>
            <${Icon} name="search" size=${14} />
            <span>Jump to…</span>
            <${Kbd}>${hotkeyLabel('mod+k')}<//>
          </button>
          <div class="topbar-spacer"></div>
          <div class="topbar-actions">
            <${LiveChip} />
            ${isPhone && html`<${IconButton} icon="search" label="Search pages and actions" onClick=${openPalette} />`}
            ${!isPhone && html`<${ThemeMenu} />`}
            ${!isPhone &&
            html`<${Menu}
              label="Session"
              icon="lock"
              items=${[
                { heading: 'Admin session' },
                { label: 'Sign out', icon: 'logout', onSelect: () => signOut() },
              ]}
            />`}
          </div>
        </header>
        <main id="main" ref=${main} class="content" tabindex="-1">
          ${entry
            ? html`<${PageBoundary} key=${entry.path}><${PageHost} entry=${entry} route=${route} /><//>`
            : route.path === '/'
              ? html`<${PageSkeleton} />`
              : html`<${NotFound} route=${route} />`}
        </main>
      </div>
      <${BottomBar} currentPath=${currentPath} moreOpen=${moreOpen} onMore=${() => setMoreOpen(true)} />
    </div>
    <${MoreSheet} open=${moreOpen && isPhone} onClose=${() => setMoreOpen(false)} currentPath=${currentPath} version=${version} />
    <${CommandPalette} open=${paletteOpen} onClose=${() => setPaletteOpen(false)} commands=${commands} />
    <${ConfirmHost} />
    <${Toaster} />
  `;
}
