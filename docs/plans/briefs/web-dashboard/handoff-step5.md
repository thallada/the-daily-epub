# Step 5 handoff — settings

## Landed

- `src/web/dashboard/settings.rs`: a schema derived by walking serialized
  default and live `Config` values, including optional/env-only fields, exact
  field kinds and enum choices, derived environment names, source tracking,
  README/example-backed help text, stable group order, and dotted-path anchors.
- `/dashboard/settings`: reload-on-mtime with a visible load-error banner and
  last-good config retention; secret presence-only rendering, environment
  locks, typed inputs, defaults/reset controls, and weight normalization notes.
- Typed `toml_edit` saves that preserve comments/order, collect field errors,
  write multiline string arrays, create missing tables, validate through
  `Config::load` in `<path>.tmp.<pid>`, preserve permissions, rename atomically,
  swap the live config, and record one attributed `config_changes` row per key.
- Provider add/remove flows through the same validated writer, including name
  and kind validation, role-reference refusal, placeholder defaults, audit
  history, and redaction of a hand-written provider `api_key` from history.
- `/dashboard/settings/history`, newest first at 100 rows per page.
- A step 5 CSS block and reset helper in `app.js`; `/etc/daily-epub` added to
  the server unit's `ReadWritePaths`.
- Sixteen settings tests covering every schema and writer case listed in §17,
  including router-level rendering and POST behavior through `oneshot`.

## Deviations and notes

- Shipped providers cannot be removed, even when unreferenced. They are members
  of `Config::default()`, so deleting their TOML table would immediately restore
  the built-in entry and falsely report success. They can be edited or left
  unreferenced; custom providers can be removed normally.
- `FieldKind::Enum` owns `Vec<String>` rather than the plan sketch's static
  slice because `llm.bulk` and `llm.editor` must include provider names derived
  at runtime. The rendered behavior and validation table are unchanged.
- Restart notices also cover `server.public_url`, `server.session_days`,
  `server.login_attempts`, and `server.login_window_minutes`, in addition to
  the plan sketch's `database_path` and `server.bind`, because those values are
  captured when the server/session/throttle layers are built.
- An absent key whose effective value is already the submitted default remains
  absent. This reconciles “only changed fields” with the no-JavaScript form
  posting every editable field; resetting an explicit non-default file value
  still writes the default explicitly.
- Step 6 should call `WebState::reload_if_changed` before starting each job, as
  §4.2 requires. Step 5 supplies the shared helper; this branch's jobs module is
  intentionally still the parallel-step stub.
- The shared brief's sandbox list omits three pre-existing OpenAI fake-server
  tests that bind through the same `curate::llm::tests::serve` helper as its four
  named Anthropic tests. The step 1 handoff records those three additional
  sandbox failures; they are not settings regressions.

## Verification

- `cargo fmt`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- Focused `cargo test web::dashboard::settings -- --nocapture`: **16 passed,
  0 failed**.
- Unfiltered `cargo test`: **379 library tests passed** before the harness
  reported the expected loopback-bind failures (the 10 named library tests plus
  the three OpenAI tests noted above); no settings test failed.
- `cargo test` with all pre-existing loopback listener tests excluded:
  **411 passed, 0 failed, 15 filtered out** (379 library, 7 binary-unit, and 25
  integration tests passed; 13 library listener tests and both `m7_server`
  tests were filtered).
