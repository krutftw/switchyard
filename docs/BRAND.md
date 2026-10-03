# Switchya

Switchya is the name of the coding app and its CLI. The domain is `switchya.com`. The standalone Rust API gateway keeps the name **Switchyard Gateway** and the command `switchyard`. Use **Powered by Switchyard Gateway** when explaining the relationship.

The app command is `switchya`. Internal crate names and existing gateway configuration names do not need to follow the product name. Do not rename gateway APIs, configuration files, released artifacts, or provider settings as a branding exercise.

## Product language

- **Switchya:** the app, its workspace, and its CLI.
- **Switchya native engine:** the app's own coding-agent runtime, available in the early preview.
- **Codex integration:** the separate structured adapter for a user's Codex setup. It is not a claim of affiliation with OpenAI.
- **Switchyard Gateway:** the independently usable API gateway. The app's native engine can use it for model access. Do not imply every integrated agent necessarily routes through it.

Lead with the work: a project, a task, a change, a review. Write in short, specific sentences. Explain uncertainty where it affects a decision. Avoid “best,” “unlimited,” “fully autonomous,” cost-saving percentages, and claims about competitors without comparative evidence.

The app and CLI are an **early Windows portable preview**. State the verified scope and the remaining live-model and account checks; do not equate fixture tests with real provider validation. Platform targets are Windows, macOS, and Linux; targets are not equivalent to downloadable, verified releases. The public Gateway v0.1.0 Windows and Linux x64 downloads are a different product and must be labeled accordingly. macOS source checks are not equivalent to a tested Mac build.

## Visual identity

Use the original mark in `app-ui/assets/switchya-mark.svg`: an angular S-shaped route with a branch, on a graphite tile. The website keeps an exact copy under `site/public/assets/`. Native packaging derives its icon from this source. The mark must remain legible at small sizes; do not add glows, bevels, or extra outlines.

| Token | Value | Purpose |
| --- | --- | --- |
| Graphite | `#101820` | Project rail, architecture section, strong text |
| Sheet | `#f6f8fb` | Quiet page and workspace background |
| Paper | `#ffffff` | Main content and conversation surface |
| Steel | `#dce3eb` | Structural separators |
| Signal blue | `#176fa8` | Focus, selection, and primary action |
| Hold amber | `#97600c` | Genuine development or approval status |

Archivo is the interface, prose, and display face. Use its variable width deliberately for compact display text. JetBrains Mono is reserved for commands, file paths, and code. The self-hosted font files include their OFL notices. The site does not load remote fonts.

## Website design plan

The characteristic visual is a continuous coding workspace. Lead with the concrete product: a desktop coding workspace and CLI, with the user's choice of model providers. The main headline is **A coding workspace. Your choice of models.** Follow it with the project, provider setup, and proposed changes; explain the Gateway only after the visitor understands the app.

Keep the graphite, sheet, paper, steel, signal blue, and hold amber tokens above. Archivo carries the headline and prose; JetBrains Mono is reserved for real command syntax. All copy is left aligned. Use one workspace illustration, a compact definition list, and a useful connection diagram. Do not turn every capability into a card or add a repeated closing sales banner.

```text
mark + Switchya                       Workspace  Model access  Downloads

Windows preview
A coding workspace.                   A desktop app and CLI for your code,
Your choice of models.                using the providers you choose.

┌──────────── Workspace / CLI / Gateway ─────────── Interface concept ─────┐
│ Projects       │ Task and conversation                  │ Review         │
│                │                                        │                │
└──────────────────────────────────────────────────────────────────────────┘

Work from an existing project    Local project / provider setup / changes

Use your providers.              Native engine → Gateway → API providers
Or your Codex setup.             Codex integration → your Codex setup

Switchyard Gateway              Actual download, setup, protocol details

Downloads and status             Windows app / CLI preview; Gateway release
```

Content stays left aligned and prose measures stay below roughly 75 characters. The hero preview is explicitly labeled as an interface concept until replaced by a screenshot of a verified build. It contains no invented customers, success counts, test results, or activity feed. Its only live controls switch explanatory views.

Design review, 3 October 2026: the first version repeated vague phrases about ideas, work, and future plans. The refinement replaces those with specific jobs, removes the repeated closing banner and decorative review skeleton, and gives the Gateway download an explicit label. The light illustration remains a concept, not an actual app screenshot. No invented conversation, successful task, customer count, or testimonial is used. Named account profiles and defaults have been exercised; real sign-ins and simultaneous authenticated account operation remain unverified.

The refined website was visually approved and published on 3 October 2026 as version `83a8dfc2-9a22-4688-9258-86d4b4abf056`. See `site/DESIGN-REVIEW.md` for review images and checks, and `site/VERIFICATION.md` for the public read-back evidence.

## Accessibility and publication

Keep keyboard focus visible, touch targets at least 44 CSS pixels, and the page readable at narrow widths and 200% zoom. Tabs use keyboard navigation and announce selection. The page's content remains available without JavaScript. Respect reduced-motion preferences and avoid automatic movement.

The website has no account signup, mailing-list form, analytics, or tracking scripts. Do not imply those services exist. Do not add an app download link until an actual app artifact is available. Verify source links against published branches before publication. Domain ownership does not mean the website has been deployed.
