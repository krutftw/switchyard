// API keys: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "API keys"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Keys() {
  return html`
    <${Page} title="API keys" description="Keys that client applications use to call this gateway.">
      <${Panel}>
        <${EmptyState} icon="key" title="Coming soon" description="Client keys with their limits, allowed models and usage will be shown here." />
      <//>
    <//>
  `;
}
