# Dashboard tables CSS handoff

## What changed

- `src/web/tailwind.css`
  - Disabled Tailwind v4 automatic repository scanning with
    `@import "tailwindcss" source(none);`; retained the explicit template and
    `app.js` sources.
  - Replaced the padded/max-width dashboard container with the specified named
    `content`/`wide` grid and 1rem/1.5rem gutter variable.
  - Let direct-child `.scroll-x` wrappers occupy the wide track and made their
    tables shrink-to-fit between the content-column floor and wrapper ceiling.
  - Removed the global `.scroll-x > table { min-width:max-content; }` rule.
  - Kept `.num` alignment/tabular figures on headers and cells, while limiting
    nowrap to `td.num`.
  - Raised `.cell-wrap`'s maximum width from 24rem to 32rem.
  - Added a pointer from the table rules to `docs/dashboard-tables.md`.
- `src/web/static/app.css`
  - Rebuilt from the Tailwind source and checked for reproducibility.

No templates, JavaScript, reader table rules, preview rules, or rating-cell
rules changed.

## Generated CSS diff audit

Compared committed and rebuilt CSS one closing brace per line with:

```sh
diff -U0 \
  <(git show HEAD:src/web/static/app.css | tr '}' '\n') \
  <(tr '}' '\n' < src/web/static/app.css)
```

The 177-line diff contained 111 removed and 12 added content lines (excluding
the two diff file headers). The component-rule changes were exactly the table
framework changes above: remove `.scroll-x > table`, replace `.dashboard`, add
the three direct-child dashboard selectors, split `td.num` nowrap from `.num`,
and change `.cell-wrap` from 24rem to 32rem. The remainder was removal of
utilities, unused theme symbols, and supporting custom properties that had
previously been emitted by repository-wide automatic source discovery. There
were no unrelated added selectors.

The rebuilt file shrank from 77,721 to 70,697 bytes. The documentation-only
utility selectors `.line-clamp-3`, `.max-w-[32rem]`, `.min-w-[10rem]`, and
`.pb-6` are absent. The semantic `.cell-wrap` component still contains
`min-width:10rem` and `max-width:32rem`, and `.dashboard` still contains its
requested `padding-bottom` declaration.

## Verification

- `npm run css` — passed.
- `npm run css:check` — passed with no diff.
- `cargo build` — passed after the final CSS build.
- `cargo test --lib web::` — 135 passed; the two failures were
  `web::dashboard::feeds::tests::a_duplicate_subscription_still_decides_the_row`
  and `web::dashboard::feeds::tests::add_subscribes_and_records_the_miniflux_feed_id`.
  Both failed at the test bind setup with the documented
  `PermissionDenied` / `Operation not permitted` sandbox error.
- `cargo fmt` — passed without changes.
- `cargo clippy --all-targets -- -D warnings` — passed.
- `git diff --check` — passed.

An extra full `cargo test` required by the shared brief was attempted, but the
filesystem ran out of space while creating another test archive before tests
started. I removed only this package's generated artifacts with
`cargo clean -p daily-epub` (2.0 GiB), then reran the complete user-requested
command sequence above successfully. The full suite remains unverified; the
focused web suite is verified subject only to the two allowed bind failures.

## Deviations and open items

There are no implementation deviations. I did not run the browser measurement
script because the sandbox cannot bind a preview server, as independently
confirmed by the focused test failures. Visual viewport measurement remains for
the post-merge environment described in the shared brief.
