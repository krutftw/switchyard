// Logs: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "Logs (live tail)"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Logs() {
  return html`
    <${Page} title="Logs" description="The gateway’s own log, as it is written.">
      <${Panel}>
        <${EmptyState} icon="logs" title="Coming soon" description="The live log tail with level and text filters will be shown here." />
      <//>
    <//>
  `;
}
