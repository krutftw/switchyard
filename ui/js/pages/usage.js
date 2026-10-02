// Usage: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "Usage (charts, cost)"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Usage() {
  return html`
    <${Page} title="Usage" description="Requests, tokens and estimated cost over time.">
      <${Panel}>
        <${EmptyState} icon="usage" title="Coming soon" description="Charts by model, provider and client key, and the price table, will be shown here." />
      <//>
    <//>
  `;
}
