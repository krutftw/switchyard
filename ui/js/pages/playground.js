// Playground: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "Playground (any protocol, streaming, raw events)"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function Playground() {
  return html`
    <${Page} title="Playground" description="Send a request through the gateway in any protocol and watch the answer.">
      <${Panel}>
        <${EmptyState} icon="playground" title="Coming soon" description="The request editor, streaming output and raw event view will be shown here." />
      <//>
    <//>
  `;
}
