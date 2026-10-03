# Switchya website design review

3 October 2026. This revision was approved after desktop and phone visual review and published as version `83a8dfc2-9a22-4688-9258-86d4b4abf056`.

The later Windows-preview availability update keeps this design. Its staged status is recorded in [VERIFICATION.md](./VERIFICATION.md); the review below describes the original published design.

Live: [switchya.com](https://switchya.com/). Local preview: http://127.0.0.1:18742/

## What changed

- The opening now identifies the product directly: **A coding workspace. Your choice of models.**
- The introduction names the desktop app, CLI, provider choice and review workflow. Navigation uses Workspace, Model access and Downloads.
- The workspace explanation uses a two-column layout with three practical jobs, replacing the generic principles.
- The illustrative workspace has a useful empty review state instead of decorative skeleton lines. It still makes no claim to be a released app screenshot.
- The connection diagram explains the native-engine/Gateway route and separate Codex connection in fewer words.
- Gateway actions now say what they open or download. The app and CLI remain clearly under development.
- Removed the repeated closing sales banner and separate platform row. Platform targets remain in the availability note.
- Kept the existing SVG mark, Archivo/JetBrains Mono, light workspace, graphite rail and restrained blue. Reduced the preview shadow and loosened headline spacing.

There are no invented usage figures, customer quotes, task successes, app download links or claims that account switching has shipped.

## Visual review

The desktop opening, full desktop page and full phone page were reviewed before this design revision was published. Review captures are not distributed with the source.

## Checks

The static website checks pass: two HTML pages, 18 local references, 12 files. The local page was inspected in Edge at default desktop width, 768×1024, 390×844 and 320×780, with no horizontal overflow. Tab selection worked with mouse and ArrowRight, End and Home; selected tab, keyboard focus and visible panel agreed. The temporary viewport override was reset.

No site-origin warnings/errors appeared in the recorded console. Browser extensions did emit messages, so the entire browser console is not described as empty. Zoom, screen-reader testing and a full accessibility audit remain unverified.

## App capture review

An actual app capture was inspected after the website deployment. It shows a completed controlled fixture run, not evidence of a real-provider production task. No app UI files were edited in this website pass.

The frontend owner already has the tool-only placeholder, raw event names/status, command-result presentation and change-count issues. Three additional usability refinements were sent to root:

- The two visible session titles truncate to the same prefix. Allow two lines or another way to expose the distinguishing task text.
- The idle composer occupies about 184 pixels of the 720-pixel-high capture. Reduce its resting height toward 110–130 pixels and expand as the user types, preserving comfortable controls.
- The approval-policy hint is much smaller than normal UI text. Make this meaningful execution constraint readable at the normal secondary-text size.

This capture shows `Changes 1`; a zero-change-count issue cannot be inferred from this image.
