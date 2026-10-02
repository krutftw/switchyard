// Overview: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "Overview (live)"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Overview() {
  return html`
    <${Page} title="Overview" description="What the gateway is doing right now.">
      <${Panel}>
        <${EmptyState} icon="overview" title="Coming soon" description="Live traffic, health per provider and the latest errors will be shown here." />
      <//>
    <//>
  `;
}
