// Models: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "Models (routes + aliases)"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Models() {
  return html`
    <${Page} title="Models" description="The model names clients can ask for and where each one is routed.">
      <${Panel}>
        <${EmptyState} icon="models" title="Coming soon" description="The model table, its routes and the alias editor will be shown here." />
      <//>
    <//>
  `;
}
