# Switchya website verification

Updated 3 October 2026. This verifies the product website, not the Switchya app or CLI.

## Published Windows preview update

The website publishes Windows x64 portable preview `0.1.0-preview.1`, including the native desktop shell and CLI, while keeping the reviewed design. The release tag is `switchya-v0.1.0-preview.1` and the asset is `switchya-0.1.0-preview.1-windows-x86_64-portable.zip` in the existing `krutftw/switchyard` repository. Both were confirmed public before deployment. The independently downloaded ZIP contains 20,335,861 bytes and matches SHA-256 `ea313361bcdf7fa4c1b8a7d9bd17d5870d6301907b7825c6871b55310316c8da`.

The PowerShell examples match the current CLI parser. Existing Gateway link destinations are retained. Named Codex and Claude defaults are described; Claude remains an external official-terminal integration, without a structured workspace conversation or quota display. Live account sign-in and subscription usage remain unverified. Other platforms are offered as source; no macOS binary is claimed.

The download requirements specify Windows 10/11 x64 and separately installed Microsoft Visual C++ v14 x64 runtime and Microsoft Edge WebView2 Runtime (desktop shell). The portable ZIP has no installer or bundled runtime DLLs. Microsoft's [supported Visual C++ runtime page](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170) was checked for the runtime name and x64 package; the app's binary dependency was supplied by the release verification owner. Setup is linked through the preview release notes.

`asset-manifest.json` tracks the published files. The deployment version, hashes and read-back below cover the live preview download copy.

The update passed `npm run check`: two HTML pages, 18 local references and 12 static files. Edge checks at the normal desktop viewport, 390×844 and 320×780 showed no horizontal overflow. The Windows button, download section and PowerShell panel remained readable, and Workspace/CLI selection worked. Temporary viewport overrides were reset. Public download availability and the final live browser page were verified after publication.

The manifest records **186,934 bytes** across 12 files. `index.html` is `827356a1fd836ee1b9235b99241b779880f18ecd9949ca8619618fc5fbf326be`; CSS, JavaScript, fonts and the mark are unchanged from the earlier design revision. No site-origin warnings/errors were observed; browser extensions emitted unrelated messages.

## Live design revision

The refined design was visually reviewed and approved, then published to [switchya.com](https://switchya.com/) and [www.switchya.com](https://www.switchya.com/).

- Cloudflare Workers Static Assets version: `8fa04428-9edc-4219-a54b-70d44439eb5c`.
- Both custom domains are configured; `workers_dev` is disabled. No Worker script, account ID in source, storage binding, or backend is configured.
- The apex is canonical. www serves the same content; HTTP requests to each host redirect to its corresponding HTTPS URL.
- The approved HTML/CSS and every manifest file were hash-checked immediately before publication. Wrangler 4.147.0 deployment dry run and publication succeeded.

## Public read-back

At **2026-10-03 09:59:03 UTC**, both homepages, CSS, JavaScript, the SVG mark, self-hosted fonts and notices, robots file and sitemap returned HTTP 200 and matched their local SHA-256 hashes. An unknown path on both domains returned the exact custom page with HTTP 404. Both HTTP hostnames returned 301 to HTTPS.

| File | SHA-256 |
| --- | --- |
| `index.html` | `827356a1fd836ee1b9235b99241b779880f18ecd9949ca8619618fc5fbf326be` |
| `site.css` | `20bf686ed9390fe3915bcb5aa61b6456b5e09a31f7072fe7109d5246f5bca489` |
| `site.js` | `cc15a4c69b7770ab863771e1d1adc8c80ffa498f6cbdddda00d95eaf2b2e7619` |

Homepage, CSS, JavaScript, SVG and missing-page responses expose `Cache-Control: public, max-age=0, must-revalidate, no-transform`. Fonts expose exactly `public, max-age=2592000, no-transform`, without conflicting max-age values. All checked HTTPS responses include the configured same-origin CSP, `X-Content-Type-Options: nosniff` and `X-Frame-Options: DENY`. No Cloudflare analytics beacon is present in the public HTML.

Machine-readable evidence is saved in [deployment-readback.json](./deployment-readback.json). The published revision contains 12 public files totaling **186,934 bytes**, including fonts, notices and header configuration, tracked by [asset-manifest.json](./asset-manifest.json). The mark remains byte-identical to the app SVG.

## Design and browser checks

Root reviewed the desktop and phone images before publication. See [DESIGN-REVIEW.md](./DESIGN-REVIEW.md) for the changes and captures.

The exact deployed source was checked locally in Edge at default desktop width, 768×1024, 390×844 and 320×780, with no horizontal overflow. Mouse tab selection and ArrowRight, End and Home keyboard selection worked; selected tab, focus and visible panel agreed. Temporary viewport overrides were reset.

No site-origin browser warnings/errors were recorded. Installed browser extensions emitted their own messages, so the entire browser console is not described as empty.

`npm run check` passed, covering two HTML pages, 18 local references and 12 static files. JavaScript syntax, duplicate IDs, local references, anchors, panel targets and the exact custom-domain allowlist are checked. The configuration checks reject a Worker entry point or account ID in source and require `workers_dev: false`.

Thirteen selected small-text color pairs were calculated during preparation and exceeded 4.5:1 after correction, with a minimum of 4.69:1. That targeted calculation is not a full accessibility audit. No claim is made for 200% zoom, every keyboard path, screen-reader behavior or all browsers.

## Product boundaries

The live site describes a Windows x64 portable preview containing the app and CLI. Preview views remain explicitly illustrative layouts. Other app platforms are source-only. Named account defaults are described with the live-authentication, usage and Claude-terminal limits above.

The website retains the separate public Switchyard Gateway v0.1.0 Windows x64 and Linux x64 release links. Switchya is a portable ZIP, not an installer. There is no signup backend, pricing, invented customers, testimonials, adoption counts or unsupported competitor-superiority claim.

## Future authorized updates

From the site directory, run `npm ci --no-audit --no-fund`, `npm run check`, `npx wrangler --version`, `npx wrangler whoami`, and `npx wrangler deploy --dry-run` before `npm run deploy`. Resolve failing checks and complete visual review first. After publication, recheck both domains, assets/fonts, custom 404, HTTPS redirects, cache/security headers and local/public hashes. Refresh the manifest, read-back evidence and deployed version whenever public files change.
