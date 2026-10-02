// Portal: render children into a node appended to <body>, so overlays escape
// any overflow: hidden or stacking context of the place they are declared.
//
//   html`<${Portal}><div class="menu">...</div><//>`
//
// The vendored Preact build has no createPortal, so this uses a second render
// root. Two consequences worth knowing:
//   - context does not cross the portal (the dashboard uses module-level
//     stores instead of context, see lib/store.js);
//   - an error boundary above the portal does not catch errors below it.

import { render, useLayoutEffect, useRef } from '../../vendor/preact-htm.js';

export function Portal({ children }) {
  const host = useRef(null);
  if (host.current === null && typeof document !== 'undefined') {
    host.current = document.createElement('div');
    host.current.className = 'portal';
  }

  // Mount and unmount the host node.
  useLayoutEffect(() => {
    const node = host.current;
    document.body.appendChild(node);
    return () => {
      render(null, node);
      node.remove();
    };
  }, []);

  // Re-render the children whenever the owner renders.
  useLayoutEffect(() => {
    render(children, host.current);
  });

  return null;
}
