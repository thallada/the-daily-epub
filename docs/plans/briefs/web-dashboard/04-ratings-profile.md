# Step 4 — Ratings page and profile page

Read `00-shared.md`, the plan (§3, §10, §11, §14.1 for the job names the
profile page references, §16, §17 "Ratings contributions" and "Profile"), the
curation v2 plan §6, §9.2–§9.3, §11 (profile/learned adjustments), and the
handoffs for steps 1–3. This is plan §19 item 4. Reuse `signals::decay`,
`signals::gate`, `signals::direct_feeds`, `PreferenceState::load/summary`,
`profile::parse_profile_str`, `profile::parse_interests`,
`themes::group_into_themes`, `profile::is_stale`; make
`profile::MAX_RATINGS_IN_REBUILD` `pub`.

## Deliverables

1. `src/web/dashboard/ratings.rs` + `dashboard/ratings.html` (§10): header from
   `PreferenceState::summary()` with the "no embedding" notice; the "How
   ratings enter the algorithm" `<details>` with current config values inlined
   and links to the settings groups (the settings page is step 5; link to
   `/dashboard/settings#curation.feedback` etc. — anchors the settings page will
   provide); the **Current** tab with every column in the plan's table (verdict
   + inline widget with note, article, when·by with username, note, age→decay,
   neighbour weight or "no embedding", feed credit, in prompt, in rebuild, used
   last run from `signals_json.neighbours` of the latest non-dry run), filters
   by label/source/feed/q; the **Events** tab (100 per page, `superseded`
   marking, filters by label/source/user/date range).
2. `src/web/dashboard/profile.rs` + `dashboard/profile.html` (§11): the
   `profile.md` editor (textarea, Save = origin-checked POST; reject empty or
   > 64 KB; insert the previous content into `profile_versions`; write
   atomically via `<path>.tmp` + rename preserving mode; flash), the live parsed
   preview (server-rendered on GET is fine: the passthrough sections and the
   extracted `## Interests` lines), history with 200-char previews and Restore
   (`POST /dashboard/profile/restore`), standing OPML interests grouped by
   theme (read-only, with path and count), the system prompt
   (`kv.taste_profile` in a collapsed `<pre>`), the learned adjustments block
   with its age and whether a rebuild is due, and a "Rebuild profile now"
   button that POSTs to `/dashboard/jobs/profile-rebuild` (step 6 implements
   the route; render the form now, disabled with a note if `jobs_enabled` is
   false).
3. Tests (§17): decay/weight/feed-credit/in-prompt/in-rebuild against
   hand-checked values; "used last run" counts neighbours in `signals_json`;
   profile save writes the file and a `profile_versions` row; restore swaps;
   oversized rejected; the parsed preview matches `parse_profile_str`; admin
   guard on every new route.
