## Moved 2025-07-24 — Grouped elicitation with mode switching

- [x] GE-1: Add `GroupMode` enum (`ByLint`, `ByPath`) and `GROUPING_THRESHOLD` constant to elicit.rs
- [x] GE-2: Add `group_by_lint()` — groups suggestions by `code` field, returns `Vec<SuggestionGroup>`
- [x] GE-3: Add `group_by_path()` — groups suggestions by `file` field, returns `Vec<SuggestionGroup>`
- [x] GE-4: Add `build_grouped_schema()` — builds schema with group-all headers, mode-switch entry, and individual items; groups sorted largest-first
- [x] GE-5: Add `parse_grouped_response()` — expands `all:*` entries into member IDs, detects `view:*` mode switch; returns `GroupedSelection` enum (ModeSwitch | Selected)
- [x] GE-6: Upgrade `elicit_selection()` to use grouped presentation when count ≥ threshold, with mode-switch loop (max 1 re-elicit to prevent infinite loop)
- [x] GE-7: Add tests for `group_by_lint` (same code grouped, None-code items each standalone, ordering by group size)
- [x] GE-8: Add tests for `group_by_path` (same file grouped, ordering by group size)
- [x] GE-9: Add tests for `build_grouped_schema` (group-all entries present, mode-switch entry, individual items, below-threshold stays flat)
- [x] GE-10: Add tests for `parse_grouped_response` (group expansion, mode switch detection, mixed group+individual, dedup)
- [x] GE-11: Add tests for full `elicit_selection` flow with grouping and mode switch I/O simulation
- [x] GE-12: Build and verify all tests pass (0 warnings)
- [x] GE-13: Release build and reinstall
- [x] GE-14: Update DESIGN-NOTES.md with grouped elicitation architecture
