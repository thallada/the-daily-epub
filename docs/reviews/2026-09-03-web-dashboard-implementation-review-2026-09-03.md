# Web dashboard step 5 implementation review

The final step 5 working tree implements the settings schema, editor, provider
operations, history page, reload-on-mtime behavior, and systemd write access
described by the plan. The implementation is well covered by behavior-focused
unit and router tests. The review found one security issue in the inherited
work—removing a provider could have copied a hand-written `api_key` into
`config_changes`—and fixed it with redaction plus a regression assertion.

## Critical

None.

## High

None. The provider-history secret exposure found during review is fixed in
`provider_literal`: `api_key` is removed before a whole-provider literal is
stored or rendered by the history page.

## Medium

None.

## Low

### Shipped provider removal differs from the literal plan wording

- Evidence: `ProviderCard::removable` and `remove_provider` refuse entries in
  `default_providers()`, even when no `[llm]` role references them.
- Plan difference: §13.3 only explicitly requires refusal when an `[llm]` role
  names the provider.
- Reason and recommendation: deleting a shipped provider's file table cannot
  remove it from the effective config because `Config::default()` supplies it
  again; pretending otherwise would reset it while reporting removal. Keep the
  explicit refusal unless config loading later gains a provider tombstone or
  replacement-map semantic.

### Absent defaults are not materialized by an otherwise unchanged form

- Evidence: `plan_changes` compares an absent file value through its effective
  default before deciding whether to write.
- Plan difference: §13.2's “explicit beats implicit” sentence can be read as
  requiring an absent default-valued key to be written whenever posted.
- Reason and recommendation: the same section says the form carries every
  editable field and only changed fields are written. Materializing every
  absent default on any no-JavaScript save would violate that behavior. Keep
  the effective-value comparison; a reset from an explicit non-default value
  still writes the default explicitly. Clarify this sentence in a future plan
  revision if touched-vs-untouched browser state becomes a requirement.

## Nits

None.

## Plan Coverage

| Requirement | Status | Evidence |
|---|---|---|
| Derived schema, kinds, sources, help, group order and anchors | Implemented as planned | `schema`, `schema_with_env`, `walk`, `kind_for`, `SETTINGS_HELP` |
| Secret redaction and environment locks | Implemented as planned | `source_for`, secret field rendering, provider-literal redaction |
| Typed `toml_edit` save with collected errors | Implemented as planned | `plan_changes`, `apply_change` |
| Temp-file validation, permission preservation, rename and live swap | Implemented as planned | `write_validated`, `install` |
| One attributed history row per changed key | Implemented as planned | `record_changes`, `config_changes` |
| Provider add/remove and referenced-provider refusal | Implemented with the shipped-provider qualification above | `add_provider`, `remove_provider` |
| Reload on mtime with previous config retained after an error | Implemented as planned | `WebState::reload_if_changed`, settings GET banner |
| Settings history page, newest first, 100 per page | Implemented as planned | `history`, `config_changes`, `settings_history.html` |
| `/etc/daily-epub` writable in the server unit | Implemented as planned | `systemd/daily-epub.service` |

## Testing Assessment

Existing tests are meaningful: they compare the schema with serialized
`Config::default()`, compare shipped TOML leaves with the help table, exercise
all enum options through deserialization and `validate()`, prove secrets carry
no value, mutate and restore a real environment variable, compare untouched
configuration lines byte-for-byte, verify multiline arrays and new tables,
exercise validation failure and permission preservation, inspect persisted
history rows, add/remove/refuse providers, and drive the settings routes through
`Router::oneshot`. The provider test also proves a file-sourced API key does not
reach the audit table.

No weak or missing test from the step 5 list remains. The only environmental
suite limitation is the repository's pre-existing listener tests, which cannot
bind loopback in the sandbox.

## Open Questions

- Should the plan eventually define a tombstone for removing shipped providers,
  or should the current “leave built-ins unreferenced” behavior become the
  documented contract?
- The shared brief lists four Anthropic listener tests as sandbox-bound, while
  the step 1 handoff also identifies three OpenAI tests using the same loopback
  fake server. The latter fail with the identical sandbox permission error.
