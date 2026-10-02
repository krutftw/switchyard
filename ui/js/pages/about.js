// About: placeholder. Replace this module with the real page
// (DESIGN.md section 12: "About"). The route, navigation entry and
// title are already wired in js/routes.js; see ui/UI_GUIDE.md, "Adding a page".

import { html } from '../../vendor/preact-htm.js';
import { EmptyState, Page, Panel } from '../components/surface.js';

export default function About() {
  return html`
    <${Page} title="About" description="Version, build and licence information.">
      <${Panel}>
        <${EmptyState} icon="about" title="Coming soon" description="The gateway version, uptime, config path and third-party licences will be shown here." />
      <//>
    <//>
  `;
}
