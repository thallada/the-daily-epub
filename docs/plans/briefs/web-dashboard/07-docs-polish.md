# Step 7 — Docs, users page, polish

Read `00-shared.md`, the plan (§3, §6.1, §7.2, §18, §19 item 7, §20
acceptance criteria), the README, `config.example.toml`,
`docs/plans/2026-08-15-implementation-notes.md`, `docs/runbooks/`, and every
`handoff-step*.md`. This is plan §19 item 7.

## Deliverables

1. `src/web/dashboard/users.rs` + `dashboard/users.html`: read-only list of
   users (username, role, disabled, created, last login, open session count)
   and a note that edits happen through the `users` CLI.
2. README: the site (public vs signed-in vs admin), roles, the `users` CLI,
   jobs (unit + polkit), settings (in-place writes, env locks), the updated
   route table, the reverse-proxy note about `X-Forwarded-For`.
3. `config.example.toml`: confirm the new `[server]` keys are documented with
   comments matching the README table.
4. `docs/plans/2026-08-15-implementation-notes.md`: a "Web dashboard
   (2026-09-03)" section recording the verified facts with dates (polkit 124
   and the rule, unit changes, toml_edit in-place writes, axum-login git rev +
   tower-sessions 0.15 + our store, password-auth, tower_governor, the
   sandbox/test caveats) and the implementation-time decisions the handoffs
   recorded.
5. `docs/runbooks/web-dashboard-rollout.md` with the nine steps of plan §18,
   written for the production host.
6. Layout polish: Atom `<link rel="alternate">` and favicon in `<head>`, the
   Users link in the admin nav, a pass over dark mode and narrow screens in
   `app.css` (every wide table scrolls in `.scroll-x`; the masthead wraps; nav
   collapses to wrapping links), and the 404/500 pages in the site layout.
7. Walk plan §20's acceptance criteria one by one and fix any gap you can
   close inside this step; list the rest in your handoff.
8. Tests: the users page under the admin guard; a smoke test that every
   template referenced by a route renders with the fixture data (extend the
   existing router tests rather than duplicating setup).

## Follow-ups collected during the orchestrator's reviews of steps 1–6

- Download buttons on the full issue page show raw byte counts; render them
  human-readable (KB/MB).
- The `/files/*` deviation: with no Basic auth configured the files stay
  public (see handoff-step1 review notes); document that in the README's route
  table and the rollout runbook.
- The settings page cannot remove a shipped provider (handoff-step5); mention
  it in the README settings section.
- Every step left a handoff in this directory; fold their "left for later"
  items into the acceptance walk-through and list anything still open.
- Two Codex review documents exist under `docs/reviews/` for this plan; leave
  them as they are.
