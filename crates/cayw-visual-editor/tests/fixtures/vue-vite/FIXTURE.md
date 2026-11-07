# SYNTHETIC TEMPLATE FIXTURE — Vue 3 + Vite 7

This directory is a **from-scratch synthetic template fixture** written for
Issue 30 framework acceptance. It is not a downloaded project, not a clone,
not a modified third-party template, and not presented as an external real
project. All application source, markup, styles, and the SVG images were
written for this fixture.

## Provenance and license

This fixture was authored by the CAYW project for Issue 30. The fixture source
in this directory is licensed under Apache-2.0, the repository's license (see
the repository `LICENSE`). No third-party project code is included.

It intentionally has no editor instrumentation and no `cayw` markers.
`data-testid` attributes and static ids are fixture selectors for Playwright
only. Scoped styles are real SFC `<style scoped>` blocks.

## Source layout

| Path | Purpose |
| --- | --- |
| `src/App.vue` | Composes multiple SFCs |
| `src/components/SiteHeader.vue` | Header, native links, one `SharedLabel` instance, scoped style |
| `src/components/HeroSection.vue` | Native static title, description, logo image, CTA button, container, scoped style |
| `src/components/FeatureGrid.vue` | Container with two static native cards plus an empty insertion target, scoped style |
| `src/components/SecondaryNote.vue` | Imported component rendered once; owns `#secondary-title` in a different file, scoped style |
| `src/components/SharedLabel.vue` | Repeated component used in header and footer; only a class, no DOM id, scoped style |
| `src/components/CounterForm.vue` | Dynamic counter button and form interaction, scoped style |
| `src/components/DynamicPanel.vue` | `v-for` list, `v-if` content, dynamic `:style` paragraph, scoped style |
| `src/components/SiteFooter.vue` | Static footer and second `SharedLabel` instance, scoped style |
| `public/logo.svg` | Original synthetic SVG logo |
| `public/alternate.svg` | Second original SVG used to verify image source replacement and decode |
| `src/styles.css` | Global fixture styles; responsive at 390 px |

## Required observable selectors

| Selector | Element |
| --- | --- |
| `#hero-title` | Static native `h1` |
| `#hero-description` | Static native `p` |
| `#primary-action` | Static native `button` with fixed text |
| `#original-image` | Static native `img` with `src="/logo.svg"` |
| `#static-cards` | Static card container |
| `#first-card` | Static native `article` card |
| `#second-card` | Static native `article` card |
| `#insertion-target` | Empty `div` insertion target |
| `#secondary-title` | Static native `h2` in `SecondaryNote.vue`, a different source file |
| `.shared-label` | Two instances from `SharedLabel.vue`; no duplicate DOM id |
| `#counter` | Native `button`; its text includes the current count and clicking increments it |
| `#name-input` | Native form input |
| `#form-output` | Dynamic form output |
| `#dynamic-list` | List container whose children come from `v-for` |
| `#dynamic-style` | Native `p` with a dynamic `:style` expression |

Additional fixture selectors:

| Selector | Element |
| --- | --- |
| `[data-testid="app-root"]` | Outer app shell |
| `[data-testid="page-container"]` | Main container |
| `[data-testid="site-header"]` | Header container |
| `[data-testid="brand-link"]` | Static native link |
| `[data-testid="hero-section"]` | Hero container |
| `[data-testid="feature-card"]` | Cards; use `data-feature-id` to disambiguate `clarity`, `safety`, `speed` |
| `[data-testid="counter-value"]` | Dynamic counter output |
| `[data-testid="counter-increment"]` | Increment button |
| `[data-testid="counter-decrement"]` | Decrement button |
| `[data-testid="contact-form"]` | Native form |
| `[data-testid="form-submit"]` | Form button |
| `[data-testid="details-toggle"]` | Conditional toggle |
| `[data-testid="status-banner"]` | Conditionally rendered element |
| `[data-testid="dynamic-item"]` | Loop-rendered item; `data-dynamic-id` is `one`, `two`, `three` |
| `[data-testid="static-footer"]` | Static footer |

## Commands

```sh
npm ci
npm run build
npm run preview -- --port 4174
```

Expected build output: `dist/index.html`, `dist/assets/*.js`, `dist/assets/*.css`,
`dist/logo.svg`, and `dist/alternate.svg`.
