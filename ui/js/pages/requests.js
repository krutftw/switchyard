// Requests: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "Requests (live table + detail drawer)"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Requests() {
  return html`
    <${Page} title="Requests" description="Every request the gateway served, newest first.">
      <${Panel}>
        <${EmptyState} icon="requests" title="Coming soon" description="The live request table and the detail drawer with attempts and captured bodies will be shown here." />
      <//>
    <//>
  `;
}
