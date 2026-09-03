# Step 2 — Full issue views, rating widget, POST /rate

Read `00-shared.md`, then the plan `docs/plans/2026-09-03-web-dashboard.md`
(§3, §4.3–§4.4, §5.4, §6.3–§6.4, §8 including §8.1, §16, §17), then
`handoff-step1.md` for what the foundation actually shipped. This is plan §19
item 2. Build on the step 1 code as it exists; do not restructure it.

## Deliverables

1. `issue_full.html` (§8): masthead + dateline, The Brief (`front_page_html`
   `|safe`), download buttons (from `IssueView.downloads`), the per-section
   index like the EPUB's In-this-issue page (title → article page,
   `source · N min read`, summary, Why it's here), the rating widget for admins,
   links to World Briefing / Behind the paper (only when present), colophon
   facts footer. `/` and `/issues/{date}` render this for any signed-in viewer.
2. `article.html` at `/issues/{date}/articles/{article_id}` (§8): header
   (title → source, byline, meta line, Why, social line via
   `chapters::social_line`, summary), body from `ammonia::clean` with the same
   configuration the EPUB uses (find it in `src/html.rs`/`src/epub`; do not run
   `to_xhtml`), `<img>` keep remote `src` and get `loading="lazy"
   referrerpolicy="no-referrer"`, the discussion via `comments::render_xhtml`
   when present, prev/next in issue order, "Read online ↗", rating widget for
   admins. 404 (site error page) when the article is not in that issue.
3. `world.html` (`/issues/{date}/world`, `world::render_xhtml`, 404 when
   absent) and `behind.html` (`/issues/{date}/behind`, the same lines as the
   EPUB chapter via `chapters::behind_*_line`; near misses link to
   `/dashboard/articles/{id}` for admins).
4. These three routes sit under the `login_required!` layer (§3).
5. The HMAC confirmation page (`server::confirmation_page`) gains the
   "Open this issue on the site" link (§8).
6. Rating widget partial `_rating_widget.html` + `src/web/rate.rs` (§8.1):
   `POST /rate` under the admin layer and the origin check; form and JSON
   variants; `RatingEvent { source: "dashboard", user_id: Some(viewer.id) }`;
   `cleared` writes value 0.0 with label `cleared`; `issue_date` optional with
   the dashboard fallback (`db::latest_issue_date_for_article`); JSON →
   `200 {"article_id","label","event_id"}`, form → flash + 303 to a validated
   same-site `next`. Check how the CLI (`main.rs` `cmd_ratings`) writes the
   `cleared` event and match it exactly.
7. `app.js`: intercept `.rating` forms, `fetch` with
   `credentials: "same-origin"` and `Accept: application/json`, update the
   active button; keep the no-JS form path working.
8. Tests (§17 "Full rendering", "Rating"): signed-in `/issues/{date}` contains
   Brief, summaries and why; article page contains body and discussion; the
   fallback loader (no `issue_json`) renders without world/discussion links;
   downloads listed only for files that exist; `POST /rate` as admin appends a
   `dashboard` event with `user_id`, as user → 403, anonymous → redirect/401,
   JSON and form variants, `next` validated, `cleared` value 0.
