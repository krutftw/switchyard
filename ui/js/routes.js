// The route table: the one list the sidebar, the phone navigation, the
// command palette and the router all read.
//
// To add a page:
//   1. create js/pages/<name>.js with a default-exported component
//   2. add an entry here
// Nothing else needs to change.
//
// Entry fields:
//   path      first segment of the hash route ("/requests"); deeper segments
//             and the query are the page's own (see lib/router.js)
//   title     nav label and default document title
//   icon      icon name (components/icons.js)
//   group     sidebar group; omit to keep the page out of the navigation
//   primary   also shown in the phone bottom bar (at most four)
//   keywords  extra words the command palette matches
//   load      () => import('./pages/<name>.js')

export const NAV_GROUPS = ['Operate', 'Gateway', 'Observe', 'System'];

export const ROUTES = [
  {
    path: '/overview',
    title: 'Overview',
    icon: 'overview',
    group: 'Operate',
    primary: true,
    keywords: 'home dashboard status live health',
    load: () => import('./pages/overview.js'),
  },
  {
    path: '/requests',
    title: 'Requests',
    icon: 'requests',
    group: 'Operate',
    primary: true,
    keywords: 'traffic stream history attempts errors inspector',
    load: () => import('./pages/requests.js'),
  },
  {
    path: '/playground',
    title: 'Playground',
    icon: 'playground',
    group: 'Operate',
    keywords: 'test try send prompt chat stream',
    load: () => import('./pages/playground.js'),
  },
  {
    path: '/providers',
    title: 'Providers',
    icon: 'providers',
    group: 'Gateway',
    primary: true,
    keywords: 'upstream credentials api keys cooldown openai anthropic gemini',
    load: () => import('./pages/providers.js'),
  },
  {
    path: '/models',
    title: 'Models',
    icon: 'models',
    group: 'Gateway',
    keywords: 'routes aliases catalog routing',
    load: () => import('./pages/models.js'),
  },
  {
    path: '/keys',
    title: 'API keys',
    icon: 'key',
    group: 'Gateway',
    keywords: 'client keys access tokens rate limit',
    load: () => import('./pages/keys.js'),
  },
  {
    path: '/usage',
    title: 'Usage',
    icon: 'usage',
    group: 'Observe',
    primary: true,
    keywords: 'tokens cost charts statistics spend latency',
    load: () => import('./pages/usage.js'),
  },
  {
    path: '/logs',
    title: 'Logs',
    icon: 'logs',
    group: 'Observe',
    keywords: 'tail application log lines debug',
    load: () => import('./pages/logs.js'),
  },
  {
    path: '/settings',
    title: 'Settings',
    icon: 'settings',
    group: 'System',
    keywords: 'config configuration toml routing streaming logging',
    load: () => import('./pages/settings.js'),
  },
  {
    path: '/about',
    title: 'About',
    icon: 'about',
    group: 'System',
    keywords: 'version build licence license',
    load: () => import('./pages/about.js'),
  },
  {
    // The component reference. Not in the navigation; reachable from the
    // command palette and at #/_kit.
    path: '/_kit',
    title: 'Component kit',
    icon: 'kit',
    keywords: 'kitchen sink components design system reference',
    load: () => import('./pages/kitchen-sink.js'),
  },
];

export const DEFAULT_ROUTE = '/overview';

/** The route entry for a parsed route, or null when nothing matches. */
export function matchRoute(route) {
  const head = `/${route.segments[0] ?? ''}`;
  return ROUTES.find((entry) => entry.path === head) ?? null;
}
