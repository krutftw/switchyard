// Settings: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "Settings (forms + raw TOML editor with validation)"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Settings() {
  return html`
    <${Page} title="Settings" description="Routing, streaming, logging and the rest of the gateway configuration.">
      <${Panel}>
        <${EmptyState} icon="settings" title="Coming soon" description="Settings forms and the raw TOML editor with validation will be shown here." />
      <//>
    <//>
  `;
}
