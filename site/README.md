# Switchya website

Static product website for `https://switchya.com`, with no build step, framework, remote fonts, analytics, or form backend. The current source stages the Windows x64 portable preview `0.1.0-preview.1`, containing the native desktop shell and CLI. Existing Gateway downloads still link to the separate public Gateway release.

The last verified deployment at [switchya.com](https://switchya.com/) and [www.switchya.com](https://www.switchya.com/) is version `83a8dfc2-9a22-4688-9258-86d4b4abf056`. The preview download copy is staged locally and is not yet deployed. Publish it only after the corresponding GitHub release and asset are public. See [VERIFICATION.md](./VERIFICATION.md) for the separate staged and published evidence.

The design refinement was approved after desktop and phone visual review and published. This update keeps that design and changes availability copy and links. See [DESIGN-REVIEW.md](./DESIGN-REVIEW.md) for the original review. `asset-manifest.json` describes the current staged public files; `deployment-readback.json` remains evidence of the last published revision.

The portable preview targets Windows 10/11 x64 and requires the [Microsoft Visual C++ v14 x64 runtime](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170). Its desktop shell also requires Microsoft Edge WebView2 Runtime. These prerequisites are installed separately; the archive has no installer or bundled runtime DLLs. The download row states these requirements and points readers to the linked release notes for setup.

## Preview and checks

Node.js 22 or newer:

```sh
node preview.mjs
# http://127.0.0.1:18742

npm run check
```

The preview server binds only to loopback and applies the site's global response headers. It does not emulate every Cloudflare routing or caching behavior. Set `SWITCHYA_SITE_PORT` to change its port. No package installation is required for this preview or the checks.

For Cloudflare's local static-asset behavior, install the pinned development dependency and use `npm run dev:cloudflare`. Stop the lightweight server first because both use port 18742.

## Hosting configuration

`wrangler.jsonc` uses Workers Static Assets with `public/` as the asset directory and `404-page` handling. Custom-domain routes attach switchya.com and www.switchya.com; workers_dev is disabled. No Worker code, account ID in source, storage binding, or backend is configured. `public/_headers` supplies the security policy. Static files and fonts are served from the same origin.

Wrangler is pinned to **4.147.0**, verified against the npm registry and Cloudflare's release on 3 October 2026. Its declared Node requirement is `>=22.0.0`. The compatibility date is `2026-10-03`.

For an authorized update after visual review, verify the account with `npx wrangler whoami`, run the checks and a deployment dry run, then use `npm run deploy`. Read back both domains, assets, custom 404, HTTPS redirects, and headers afterward. Regenerate asset-manifest.json when public files change. The global cache policy includes no-transform; at the last deployment read-back, live HTML matched its deployed source byte for byte, with no injected analytics beacon. The font rule replaces the global Cache-Control value with a 30-day cache policy.

Official configuration references checked for this implementation:

- [Static asset configuration](https://developers.cloudflare.com/workers/static-assets/binding/)
- [Custom 404 handling](https://developers.cloudflare.com/workers/static-assets/routing/static-site-generation/)
- [Static response headers](https://developers.cloudflare.com/workers/static-assets/headers/)
- [Wrangler 4.147.0 release](https://github.com/cloudflare/workers-sdk/releases/tag/wrangler%404.147.0)

## Content and assets

Brand rules live in `../docs/BRAND.md`. The SVG mark is copied unchanged from `../app-ui/assets/switchya-mark.svg`. Archivo and JetBrains Mono are copied from the app's self-hosted font files, with their OFL notices.

The hero contains labeled, illustrative interface concepts, not screenshots or recorded successful tasks. Its tabs switch explanatory views. All three views remain readable without JavaScript. The only clipboard action copies the existing `switchyard` Gateway startup command. The PowerShell CLI examples match the current native CLI parser and assume the CLI is on PATH, an existing project folder and a configured model.

The staged Switchya download targets the Windows portable ZIP at tag `switchya-v0.1.0-preview.1`; other platforms are source-only and there is no macOS binary. Named Codex and Claude defaults are described without promising verified live authentication or usage. Claude opens in its official external terminal and has no quota display. There is no mailing-list signup, pricing page, unsupported competitor claim, or fabricated adoption data. Before publication, check the release and asset anonymously. The original design passed desktop, tablet, phone and narrow-screen layout checks and mouse/ArrowRight/Home/End preview-tab navigation. Zoom, a full keyboard audit, and a full accessibility audit remain unverified.
