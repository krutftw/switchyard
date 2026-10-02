// Commands a page adds to the command palette (Ctrl/Cmd+K) while it is open.
//
//   import { useCommands } from '../lib/commands.js';
//
//   useCommands(
//     () => providers.map((p) => ({
//       id: `provider:${p.name}`,
//       label: p.name,
//       group: 'Providers',
//       icon: 'providers',
//       hint: p.kind,
//       keywords: p.base_url,
//       run: () => navigate('/providers', { query: { open: p.name } }),
//     })),
//     [providers],
//   );
//
// The shell contributes the page list and the global actions (theme, sign
// out); a page only adds what is specific to it. Commands disappear when the
// page unmounts.

import { useEffect } from '../../vendor/preact-htm.js';
import { createStore } from './store.js';

/** Extra commands currently registered by mounted pages. */
export const pageCommands = createStore([]);

/**
 * @param {() => Array<{id: string, label: string, group?: string, icon?: string, hint?: string, keywords?: string, run: () => void}>} factory
 * @param {any[]} deps  re-register when these change
 */
export function useCommands(factory, deps = []) {
  useEffect(() => {
    const mine = factory() ?? [];
    if (mine.length === 0) return undefined;
    pageCommands.replace([...pageCommands.get(), ...mine]);
    return () => pageCommands.replace(pageCommands.get().filter((command) => !mine.includes(command)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}
