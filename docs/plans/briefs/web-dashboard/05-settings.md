# Step 5 — Settings

Read `00-shared.md`, the plan (§3, §4.2 "Config reload", §13 in full, §15,
§16, §17 "Settings schema" and "Settings writer"), `src/config.rs` in full
(every struct, `validate`, `load`, `resolve_path`, `check_report`, `ENV_PREFIX`,
`ENV_SPLIT`, the `DAILY_EPUB_SECRET` alias, the stale-key checks), the README
configuration table and `config.example.toml`, and the handoffs for steps 1–4.
This is plan §19 item 5.

## Deliverables

1. `src/web/dashboard/settings.rs`: `schema(config, file) -> Vec<SettingGroup>`
   derived by walking `toml::Value::try_from(Config::default())` and the
   current config in parallel (§13.1) — one `SettingField` per leaf with
   `path`, `group`, `kind`, `current`, `default`, `source`
   (Default/File/Env(name)), `help`, `restart_required`. Kinds and the static
   enum table exactly as §13.1 item 3; `SETTINGS_HELP` seeded from the README
   table and `config.example.toml` comments, one entry per shipped key (test).
   Groups render in the §13.1 item 5 order; each group gets an `id` anchor equal
   to its dotted path (step 4 links to these).
2. `GET /dashboard/settings` (§4.2 reload-on-mtime first; report a file that
   fails to load with the `!` banner and keep the previous config live),
   `dashboard/settings.html`: `Secret` fields as set/not set, `Env` fields
   disabled with the lock note, everything else as an input with the default
   beside it and a reset affordance; weights note. `POST /dashboard/settings`
   implements §13.2 exactly: `toml_edit::DocumentMut`, only changed fields
   written ("explicit beats implicit"), typed by kind, `TextList` as a
   multi-line array with trailing comma, field errors collected before any
   write, render → `<path>.tmp.<pid>` → validate with `Config::load(Some(tmp))`
   → copy permissions → rename → swap `state.config`; one `config_changes` row
   per changed key; flash with the restart note for `restart_required` keys;
   `config_path == None` → 400.
3. Providers (§13.3): remove (refused when an `[llm]` role names it) and add
   (`[a-z0-9_]+`, must not exist, `ProviderConfig::default()` with the chosen
   kind and placeholders) through the same validate-and-rename path, with
   `config_changes` rows.
4. `GET /dashboard/settings/history` (§13.4), `dashboard/settings_history.html`.
5. `systemd/daily-epub.service`: add `/etc/daily-epub` to `ReadWritePaths`
   (the `SupplementaryGroups` line is step 6).
6. Tests (§17): every leaf of `Config::default()` appears exactly once; every
   shipped key has help; secrets are `Secret` with no value; env detection uses
   the derived name (set the env var inside the test with a unique key and
   restore it); enum options round-trip through `validate()`; starting from
   `config.example.toml` changing three keys preserves every comment and the
   order of untouched lines; a new key in an absent table creates the table;
   `TextList` multi-line; an invalid value (e.g. `deep_keep < shortlist_keep`)
   is rejected and the file untouched; permissions preserved; `config_changes`
   rows; provider add/remove; removing a referenced provider refused; reload
   on mtime swaps the config.
