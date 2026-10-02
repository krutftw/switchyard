// Providers: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "Providers"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Providers() {
  return html`
    <${Page} title="Providers" description="Upstreams the gateway can route to, and the state of their credentials.">
      <${Panel}>
        <${EmptyState} icon="providers" title="Coming soon" description="Provider configuration, credential health, connection tests and model discovery will be shown here." />
      <//>
    <//>
  `;
}
