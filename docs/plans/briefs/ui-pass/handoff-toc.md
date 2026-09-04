# Task 1 handoff — table-of-contents sidebar

## What changed

- `src/web/templates/_toc.html`
  - Reworked the narrow-screen panel to use a measured viewport offset and a
    `100dvh`-based height instead of the old `72vh` cap.
  - Added live label, count, desktop-status, link-label, and per-link progress
    hooks used by the shared TOC state updater.
  - Moved current-link presentation onto `.toc-link[aria-current]` and kept the
    server-rendered `aria-current="page"` state for chapter pages.
  - Added both hamburger and close SVGs for the open-state cross-fade and raised
    the mobile toggle/link hit areas to at least 44 px.
  - Added explicit right padding around the scrollable list.
- `src/web/templates/issue_full.html`
  - Marked The Brief, every article index item, and the colophon with matching
    `data-toc-entry` values for the front-page scroll-spy.
- `src/web/static/app.js`
  - Added mobile panel sizing from the sticky bar's current viewport position,
    recalculated on open and layout/visual-viewport resize.
  - Added mobile document scroll lock and retained toggle, Escape,
    outside-click, link-click, and `lg` breakpoint close behavior.
  - Added a passive, requestAnimationFrame-throttled front-page scroll-spy. A
    single current-link update now drives `aria-current="location"`, the mobile
    title/count, desktop chapter status, both progress bars, and TOC reveal.
  - Updated current-link reveal to use `scrollTo`, smooth unless reduced motion
    is requested, without scrolling the page.
  - Preserved progress-within-chapter behavior on article pages.
- `src/web/tailwind.css`
  - Added the dynamic mobile panel height and scroll-lock rules.
  - Added stable scrollbar gutter, right-side spacing, thin/rule-colored
    cross-browser scrollbar styling, and overscroll containment.
  - Added the centralized current/hover/focus link styles and exact 150 ms
    opacity/scale/blur hamburger-to-close transition.
- `src/web/static/app.css`
  - Rebuilt from the Tailwind source.
- `src/web/issue.rs`
  - Added each TOC item's semantic progress position (including back matter) and
    coverage for those positions and the rendered scroll-spy hooks.
  - Made the current-link router assertion insensitive to HTML attribute order.

## Design decisions

- The open panel remains positioned directly below the shared sticky bar. JS
  measures only the bar's on-screen bottom edge into `--toc-panel-top`; CSS owns
  the final `calc(100dvh - var(--toc-panel-top))` height. This handles both the
  bar still below the masthead and the bar already stuck at the top.
- The Brief uses progress position 0. Article, World, and Behind entries use
  their chapter position. The colophon uses the issue total, so reaching it
  communicates completion even though World and Behind are separate pages.
- Front-page position changes only toggle `aria-current` for current styling;
  the other text/progress surfaces are derived from that same link's label and
  progress metadata.
- Scrollbar gutter and padding are both used: the gutter reserves space for
  classic scrollbars, while padding protects labels on overlay-scrollbar
  platforms.

## Deviations and open items

- No live screenshots could be captured. The sandbox rejected the assigned
  `127.0.0.1:3601` bind with `Operation not permitted`, exactly as anticipated
  by the shared brief. Consequently desktop/mobile, light/dark, open-panel, and
  scripted-scroll visual states remain to be checked outside the sandbox.
- The full unit test run also found three OpenAI mock-server tests blocked by
  loopback binding in addition to the bind-dependent tests listed in the shared
  brief. They fail at the same listener creation point and are unrelated to this
  change.

## Verification

- `node --check src/web/static/app.js` — passed.
- `cargo fmt --check` — passed.
- `npm run css` — passed with Tailwind 4.3.3; `src/web/static/app.css` rebuilt.
- `npm run css:check` — passed with no diff. The worktree lacked its own CLI
  shim, so both npm commands used the already-installed adjacent checkout's
  `node_modules/.bin` via `PATH`; all inputs and outputs stayed in this worktree.
- `cargo test --lib web::` — 83 passed, 0 failed.
- `cargo clippy --all-targets -- -D warnings` — passed.
- `cargo test` — library phase: 431 passed, 13 bind-dependent failures. The
  failures were 7 LLM mock-server tests, the documented relative-URL extraction
  test, and the documented 5 server tests; each failed with `Operation not
  permitted` while creating a loopback listener.
- `cargo test --test config_check --test e2e_pipeline --test m2_pipeline --test m3_curation --test m4_epub`
  — 25 passed, 0 failed. `m7_server` was excluded because it requires the same
  forbidden loopback bind.
- Attempted the configured dev-server command on port 3601 — build passed; bind
  failed with `Operation not permitted`, so screenshots were not possible.
- `git diff --check` — passed.

